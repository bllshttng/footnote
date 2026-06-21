"""Tests for the backlog capture-tier store + core verbs (Wave 1.2).

Covers AC1-HP, AC1-ERR, AC1-UI, AC1-EDGE, AC1-FR, AC2-HP, AC2-ERR, AC2-EDGE.

The store functions (mint_fu_id, parse_items, scan_transcript, add_item,
write_empty_pass_artifact) are tested directly. Event emission, exit codes,
and JSON output are tested through the Typer CLI.
"""
from __future__ import annotations

import json
import threading
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

runner = CliRunner()


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()


# --------------------------------------------------------------------------
# mint_fu_id
# --------------------------------------------------------------------------

def test_mint_fu_id_shape() -> None:
    from fno.backlog.capture import mint_fu_id, FU_RE
    fu = mint_fu_id(set())
    assert FU_RE.fullmatch(fu)
    assert fu.startswith("fu-")
    assert len(fu) == len("fu-") + 6


def test_mint_fu_id_avoids_collision(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-EDGE: a freshly-minted id colliding with an existing one is re-minted."""
    from fno.backlog import capture as mod

    # First two draws collide with an existing id; the third is fresh.
    draws = iter(["aaaaaa", "aaaaaa", "bbbbbb"])

    class _FakeUUID:
        def __init__(self, h: str) -> None:
            self.hex = h + "0" * 26

    # monkeypatch.setattr auto-restores after the test, so this cannot leak
    # the fake uuid4 into other tests.
    monkeypatch.setattr(mod.uuid, "uuid4", lambda: _FakeUUID(next(draws)))
    fu = mod.mint_fu_id({"fu-aaaaaa"})
    assert fu == "fu-bbbbbb"


# --------------------------------------------------------------------------
# add_item + parse_items
# --------------------------------------------------------------------------

def test_add_item_creates_file_and_appends_shaped_line(tmp_path: Path) -> None:
    """AC1-HP + AC1-EDGE: first add creates the scaffold and appends a shaped line."""
    from fno.backlog.capture import add_item, parse_items
    inbox = tmp_path / "internal/fno/backlog/inbox.md"

    result = add_item(
        inbox,
        title="TARGET_TARGET_WORKTREE env var",
        source="PR#326",
        why="wrong worktree creates hook loop",
        where="init-target-state.sh",
        priority="p1",
    )
    assert inbox.exists()
    assert result["id"].startswith("fu-")
    assert result["priority"] == "p1"
    assert result["status"] == "open"

    text = inbox.read_text(encoding="utf-8")
    assert f"- [ ] {result['id']} - TARGET_TARGET_WORKTREE env var (p1)" in text
    assert "source: PR#326" in text
    assert "why: wrong worktree creates hook loop" in text
    assert "where: init-target-state.sh" in text

    items = parse_items(text)
    assert len(items) == 1
    assert items[0]["id"] == result["id"]
    assert items[0]["title"] == "TARGET_TARGET_WORKTREE env var"
    assert items[0]["priority"] == "p1"
    assert items[0]["status"] == "open"


def test_add_item_rejects_empty_source(tmp_path: Path) -> None:
    """AC1-ERR: empty --source is rejected."""
    from fno.backlog.capture import add_item, InboxValidationError
    inbox = tmp_path / "inbox.md"
    with pytest.raises(InboxValidationError):
        add_item(inbox, title="x", source="  ", why="y", where="z")


def test_add_item_rejects_memory_slug_source(tmp_path: Path) -> None:
    """AC1-ERR: a [[memory-slug]] source is rejected (LD2 substrate-only rule)."""
    from fno.backlog.capture import add_item, InboxValidationError
    inbox = tmp_path / "inbox.md"
    with pytest.raises(InboxValidationError):
        add_item(
            inbox,
            title="x",
            source="[[feedback_cross_worktree_target_orphan]]",
            why="y",
            where="z",
        )
    assert not inbox.exists() or "x" not in inbox.read_text(encoding="utf-8")


def test_add_item_requires_why(tmp_path: Path) -> None:
    from fno.backlog.capture import add_item, InboxValidationError
    inbox = tmp_path / "inbox.md"
    with pytest.raises(InboxValidationError):
        add_item(inbox, title="x", source="PR#1", why="", where="z")


def test_add_item_rejects_overlong_why(tmp_path: Path) -> None:
    """Failure Modes (Boundaries): reject --why longer than 120 chars."""
    from fno.backlog.capture import add_item, InboxValidationError
    inbox = tmp_path / "inbox.md"
    with pytest.raises(InboxValidationError):
        add_item(inbox, title="x", source="PR#1", why="z" * 121, where="w")


def test_add_item_rejects_bad_priority(tmp_path: Path) -> None:
    from fno.backlog.capture import add_item, InboxValidationError
    inbox = tmp_path / "inbox.md"
    with pytest.raises(InboxValidationError):
        add_item(inbox, title="x", source="PR#1", why="y", where="z", priority="p9")


def test_add_item_concurrent_writes_both_land(tmp_path: Path) -> None:
    """AC1-FR: two concurrent adds both land, neither corrupts the file."""
    from fno.backlog.capture import add_item, parse_items
    inbox = tmp_path / "inbox.md"

    errors: list[BaseException] = []

    def _worker(n: int) -> None:
        try:
            add_item(inbox, title=f"item{n}", source="PR#1", why="w", where="x")
        except BaseException as exc:  # noqa: BLE001
            errors.append(exc)

    threads = [threading.Thread(target=_worker, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert not errors, errors
    items = parse_items(inbox.read_text(encoding="utf-8"))
    assert len(items) == 8
    assert len({i["id"] for i in items}) == 8  # all distinct ids


def test_add_dedup_returns_existing_id_on_title_where_match(tmp_path: Path) -> None:
    """AC4: a second add with the same (title, where) returns the existing open
    item's id and mints no new fu-."""
    from fno.backlog.capture import add_item, parse_items
    inbox = tmp_path / "inbox.md"

    first = add_item(
        inbox, title="harden X", source="PR#1", why="w", where="src/foo.ts", priority="p2"
    )
    assert first["deduped"] is False

    second = add_item(
        inbox, title="harden X", source="PR#2", why="other why",
        where="src/foo.ts", priority="p0",
    )
    assert second["deduped"] is True
    assert second["id"] == first["id"]
    # the dedup-hit return describes the EXISTING item, not the re-file attempt:
    # priority is the stored p2, never the incoming p0 (so capture_add telemetry
    # never mislabels the item).
    assert second["priority"] == "p2"
    assert second["title"] == "harden X"
    assert second["where"] == "src/foo.ts"

    # no new fu- minted: still exactly one open item, the original
    items = parse_items(inbox.read_text(encoding="utf-8"))
    assert [i["id"] for i in items] == [first["id"]]


def test_add_no_dedup_when_where_differs(tmp_path: Path) -> None:
    """Same title, different where is a distinct capture - mint a new fu-."""
    from fno.backlog.capture import add_item, parse_items
    inbox = tmp_path / "inbox.md"
    a = add_item(inbox, title="harden X", source="PR#1", why="w", where="src/foo.ts")
    b = add_item(inbox, title="harden X", source="PR#1", why="w", where="src/bar.ts")
    assert b["deduped"] is False
    assert b["id"] != a["id"]
    assert len(parse_items(inbox.read_text(encoding="utf-8"))) == 2


def test_add_no_dedup_when_title_differs(tmp_path: Path) -> None:
    """Same where, different title is a distinct capture - mint a new fu-."""
    from fno.backlog.capture import add_item, parse_items
    inbox = tmp_path / "inbox.md"
    a = add_item(inbox, title="harden X", source="PR#1", why="w", where="src/foo.ts")
    b = add_item(inbox, title="harden Y", source="PR#1", why="w", where="src/foo.ts")
    assert b["deduped"] is False
    assert b["id"] != a["id"]
    assert len(parse_items(inbox.read_text(encoding="utf-8"))) == 2


def test_add_dedup_ignores_struck_items(tmp_path: Path) -> None:
    """A dismissed item never blocks a fresh capture - only OPEN items are dedup
    candidates."""
    from fno.backlog.capture import add_item, dismiss_item, parse_items
    inbox = tmp_path / "inbox.md"
    a = add_item(inbox, title="harden X", source="PR#1", why="w", where="src/foo.ts")
    dismiss_item(inbox, a["id"], "not needed")
    b = add_item(inbox, title="harden X", source="PR#2", why="w", where="src/foo.ts")
    assert b["deduped"] is False
    assert b["id"] != a["id"]
    # the open item is the new one; the dismissed original is struck
    open_items = parse_items(inbox.read_text(encoding="utf-8"))
    assert [i["id"] for i in open_items] == [b["id"]]


def test_add_dedup_with_no_where_matches_on_title(tmp_path: Path) -> None:
    """Two adds with no where dedup on normalized title alone (where -> '')."""
    from fno.backlog.capture import add_item
    inbox = tmp_path / "inbox.md"
    a = add_item(inbox, title="ship the thing", source="PR#1", why="w")
    b = add_item(inbox, title="ship the thing", source="PR#2", why="w")
    assert b["deduped"] is True
    assert b["id"] == a["id"]


def test_add_dedup_uses_same_normalized_key_as_tidy(tmp_path: Path) -> None:
    """Capture-time dedup shares tidy's _cluster_dedup key
    (_norm(where) + _norm(title)), so whitespace/case-only differences dedup."""
    from fno.backlog.capture import add_item
    inbox = tmp_path / "inbox.md"
    a = add_item(
        inbox, title="Harden  the  parser", source="PR#1", why="w", where="src/Foo.ts"
    )
    b = add_item(
        inbox, title="harden the parser", source="PR#2", why="w", where="src/foo.ts"
    )
    assert b["deduped"] is True
    assert b["id"] == a["id"]


# --------------------------------------------------------------------------
# scan_transcript
# --------------------------------------------------------------------------

def test_scan_finds_trigger_lines() -> None:
    from fno.backlog.capture import scan_transcript
    text = (
        "line one nothing here\n"
        "we deferred the lambda refactor\n"
        "this is a follow-up for later\n"
        "ordinary prose\n"
        "TODO: add a Literal to caller_kind\n"
    )
    cands = scan_transcript(text)
    lines = {c["line"] for c in cands}
    assert lines == {2, 3, 5}


def test_scan_empty_returns_empty_list() -> None:
    """Failure Modes (Boundaries): scan over text with no triggers returns []."""
    from fno.backlog.capture import scan_transcript
    assert scan_transcript("nothing interesting at all\njust prose\n") == []


# --------------------------------------------------------------------------
# empty-pass artifact
# --------------------------------------------------------------------------

def test_write_empty_pass_artifact(tmp_path: Path) -> None:
    """AC2-HP / AC2-EDGE: artifact has phase, session_id, entries_written, scan_candidates."""
    from fno.backlog.capture import write_empty_pass_artifact
    artifacts = tmp_path / ".fno" / "artifacts"
    p = write_empty_pass_artifact(
        session_id="SID123",
        reason="no deferrals this session",
        scan_candidates=3,
        artifacts_dir=artifacts,
    )
    assert p == artifacts / "deferrals-SID123.md"
    body = p.read_text(encoding="utf-8")
    assert "phase: deferrals" in body
    assert "session_id: SID123" in body
    assert "entries_written: 0" in body
    assert "scan_candidates: 3" in body


# --------------------------------------------------------------------------
# CLI surface
# --------------------------------------------------------------------------

def test_cli_add_emits_event_and_prints_json(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP + AC1-UI: add prints JSON to stdout and emits an capture_add event."""
    from fno.backlog.capture import cli

    res = runner.invoke(
        cli,
        [
            "add", "TARGET_TARGET_WORKTREE env var",
            "--source", "PR#326",
            "--priority", "p1",
            "--why", "wrong worktree creates hook loop",
            "--where", "init-target-state.sh",
        ],
    )
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert payload["id"].startswith("fu-")

    events_path = tmp_path / ".fno" / "events.jsonl"
    assert events_path.exists()
    types = [json.loads(l)["type"] for l in events_path.read_text().splitlines() if l.strip()]
    assert "capture_add" in types


def test_cli_add_dedup_reports_existing_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4 at the CLI: a second add with the same (title, where) exits 0 and
    prints the existing id with deduped=true (the command reports it)."""
    from fno.backlog.capture import cli

    first = runner.invoke(
        cli, ["add", "harden X", "--source", "PR#1", "--why", "w", "--where", "src/foo.ts"]
    )
    assert first.exit_code == 0, first.output
    first_id = json.loads(first.stdout)["id"]

    second = runner.invoke(
        cli, ["add", "harden X", "--source", "PR#2", "--why", "w2", "--where", "src/foo.ts"]
    )
    assert second.exit_code == 0, second.output
    payload = json.loads(
        [ln for ln in second.stdout.splitlines() if ln.strip().startswith("{")][-1]
    )
    assert payload["deduped"] is True
    assert payload["id"] == first_id


def test_cli_add_bad_source_exit_2(tmp_path: Path) -> None:
    """AC1-ERR + AC1-UI: memory-slug source exits 2 with stderr message, no event."""
    from fno.backlog.capture import cli
    res = runner.invoke(
        cli,
        ["add", "x", "--source", "[[feedback_x]]", "--why", "y", "--where", "z"],
    )
    assert res.exit_code == 2
    assert "substrate" in res.output.lower()


def test_cli_empty_pass_requires_reason(tmp_path: Path) -> None:
    """AC2-ERR: empty-pass with no reason exits non-zero."""
    from fno.backlog.capture import cli
    res = runner.invoke(cli, ["empty-pass", "--reason", "", "--session-id", "s1"])
    assert res.exit_code != 0


def test_cli_empty_pass_writes_artifact_and_event(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: empty-pass writes artifact + emits capture_empty_pass."""
    from fno.backlog.capture import cli
    res = runner.invoke(
        cli,
        ["empty-pass", "--reason", "nothing to defer", "--session-id", "SID9"],
    )
    assert res.exit_code == 0, res.output
    artifact = tmp_path / ".fno" / "artifacts" / "deferrals-SID9.md"
    assert artifact.exists()
    events_path = tmp_path / ".fno" / "events.jsonl"
    types = [json.loads(l)["type"] for l in events_path.read_text().splitlines() if l.strip()]
    assert "capture_empty_pass" in types


def test_cli_capture_pass_writes_artifact_after_adds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Capture path: after >=1 add, capture-pass seals the gate artifact with the count."""
    from fno.backlog.capture import cli
    # Seed a target-state so adds AND capture-pass detect the same session_id.
    state = tmp_path / ".fno" / "target-state.md"
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text("---\nsession_id: SIDCAP\n---\n", encoding="utf-8")

    runner.invoke(cli, ["add", "one", "--source", "PR#1", "--why", "w", "--where", "x"])
    runner.invoke(cli, ["add", "two", "--source", "PR#2", "--why", "w", "--where", "x"])
    res = runner.invoke(cli, ["capture-pass"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert payload["entries_written"] == 2
    artifact = tmp_path / ".fno" / "artifacts" / "deferrals-SIDCAP.md"
    body = artifact.read_text(encoding="utf-8")
    assert "phase: deferrals" in body
    assert "entries_written: 2" in body


def test_cli_capture_pass_zero_adds_exits_2(tmp_path: Path) -> None:
    """capture-pass with no captured items refuses (directs to empty-pass)."""
    from fno.backlog.capture import cli
    res = runner.invoke(cli, ["capture-pass", "--session-id", "SIDNONE"])
    assert res.exit_code == 2
    assert "empty-pass" in res.output


def test_cli_list_json(tmp_path: Path) -> None:
    """list --json enumerates unchecked items."""
    from fno.backlog.capture import cli
    runner.invoke(cli, ["add", "alpha", "--source", "PR#1", "--why", "w", "--where", "x"])
    runner.invoke(cli, ["add", "beta", "--source", "PR#2", "--why", "w", "--where", "x"])
    res = runner.invoke(cli, ["list", "--json"])
    assert res.exit_code == 0, res.output
    items = json.loads(res.stdout)
    titles = {i["title"] for i in items}
    assert {"alpha", "beta"} <= titles


def test_cli_list_unreadable_inbox_exits_clean(tmp_path: Path) -> None:
    """A present-but-unreadable inbox exits 1 cleanly, not a raw traceback (ab-0625107e)."""
    from fno.backlog.capture import _inbox_path, cli
    path = _inbox_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"\xff\xfe not valid utf-8 \x80\x81")  # decode error on read
    res = runner.invoke(cli, ["list"])
    assert res.exit_code == 1, res.output
    assert "cannot read inbox" in (res.stderr or res.output)
    # SystemExit from typer.Exit is clean; any other exception is the bug.
    assert res.exception is None or isinstance(res.exception, SystemExit)


# --------------------------------------------------------------------------
# scan: realistic positive + negative fixtures (Claude's Discretion #2 -
# the trigger regex must be tested with positive AND negative fixtures so a
# wrong regex returning zero candidates cannot slip through silently).
# --------------------------------------------------------------------------

def test_scan_realistic_transcript_finds_deferrals() -> None:
    """A realistic multi-line transcript with deferral phrasing returns the
    deferral lines (guards the wrong-regex-returns-zero failure)."""
    from fno.backlog.capture import scan_transcript
    text = (
        "I implemented the auth handler and the tests pass.\n"
        "We deferred the rate-limiter to a follow-up since it is p3.\n"
        "The migration looks good and is committed.\n"
        "We'll ship later, once the API stabilizes.\n"
        "TODO: replace the inline lambda with a named def.\n"
    )
    cands = scan_transcript(text)
    found = {c["line"] for c in cands}
    # lines 2 (deferred/follow-up/p3), 4 (ship later), 5 (TODO) match.
    assert {2, 4, 5} <= found
    assert 1 not in found and 3 not in found


def test_scan_prose_heavy_transcript_does_not_overmatch() -> None:
    """A prose-heavy transcript with no deferral language returns no candidates
    (guards against a regex that matches ordinary prose)."""
    from fno.backlog.capture import scan_transcript
    text = (
        "The architecture review went well and everyone agreed on the plan.\n"
        "We merged the feature branch after the build passed on the first try.\n"
        "Performance numbers were healthy and the dashboard looked clean.\n"
    )
    assert scan_transcript(text) == []


# --------------------------------------------------------------------------
# find_unparseable_fu_lines: a hand-edited line carrying a fu- token but
# failing the strict item regex must be surfaced, not silently dropped.
# --------------------------------------------------------------------------

def test_find_unparseable_fu_lines_detects_broken_separator() -> None:
    from fno.backlog.capture import find_unparseable_fu_lines
    text = (
        "# Inbox\n"
        "- [ ] fu-abc123 — well formed em-dash (p1)\n"
        "- [ ] fu-deeded - well formed hyphen (p1)\n"  # hyphen is valid post-migration
        "- [ ] fu-def456 missing the separator entirely (p2)\n"  # genuinely broken
        "just prose with no id\n"
    )
    bad = find_unparseable_fu_lines(text)
    assert len(bad) == 1
    assert bad[0][0] == 4
    assert "fu-def456" in bad[0][1]


def test_cli_list_warns_on_unparseable_fu_line(tmp_path: Path) -> None:
    """list surfaces a fu- line that fails strict parse on stderr."""
    from fno.backlog.capture import cli, _inbox_path
    runner.invoke(cli, ["add", "alpha", "--source", "PR#1", "--why", "w", "--where", "x"])
    inbox = _inbox_path()
    # Hand-edit in a genuinely broken line: a fu- token with no separator at all
    # (a hyphen would now parse fine after the Phase 2 separator migration).
    with inbox.open("a", encoding="utf-8") as fh:
        fh.write("- [ ] fu-999999 broken with no separator (p2)\n")
    res = runner.invoke(cli, ["list", "--json"])
    assert res.exit_code == 0, res.output
    assert "fu-999999" in res.output
    assert "hidden from listings" in res.output


# --------------------------------------------------------------------------
# empty-pass: the capture_empty_pass event is the gate's third honesty factor,
# so a swallowed emit must fail loudly rather than exit 0 (the
# feedback_emit_gate_transition_silent_failure class).
# --------------------------------------------------------------------------

def test_cli_empty_pass_fails_when_event_does_not_land(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.backlog.capture import cli
    import fno.events as events_mod

    def _boom(*_a, **_k):
        raise RuntimeError("simulated emit failure")

    monkeypatch.setattr(events_mod, "append_event", _boom)
    res = runner.invoke(
        cli, ["empty-pass", "--reason", "no deferrals", "--session-id", "SIDX"]
    )
    assert res.exit_code == 1, res.output
    assert "did not land" in res.output


def test_cli_empty_pass_succeeds_when_event_lands(tmp_path: Path) -> None:
    from fno.backlog.capture import cli
    res = runner.invoke(
        cli, ["empty-pass", "--reason", "no deferrals", "--session-id", "SIDY"]
    )
    assert res.exit_code == 0, res.output
    events_path = tmp_path / ".fno" / "events.jsonl"
    types = [json.loads(l)["type"] for l in events_path.read_text().splitlines() if l.strip()]
    assert "capture_empty_pass" in types


# --------------------------------------------------------------------------
# Phase 1 (US1): generalized typed-item lens.
#
# parse_managed_items + `list --by-type` recognize four token types
# (fu/cv/ab/#jc), grouped into four labeled buckets with priority + source
# section. AC1 (identify all four) and AC2 (a token in narrative prose is never
# a managed item). Inbox lines use the em-dash separator (matching the existing
# fixtures above and the real file format); the calendar emoji on the #jc line
# is written as a \U escape so the source file stays ASCII-only there.
# --------------------------------------------------------------------------

# A production-shaped fixture: a date heading + a post-merge marker, a narrative
# paragraph that mentions valid-hex tokens in running text (must NOT parse), and
# one open line of each managed type. The ab- line uses the real post-merge
# shape: priority mid-line with prose (and a second ab- token) trailing it.
_BY_TYPE_FIXTURE = (
    "---\n"
    "title: Abilities backlog inbox\n"
    "---\n"
    "\n"
    "# Abilities backlog inbox\n"
    "\n"
    "## 2026-06-03\n"
    "\n"
    "<!-- post-merge:pr-100 -->\n"
    "This merge wired the parser. In passing it referenced fu-bad999 and\n"
    "ab-feedface in the narrative, which must never parse as managed items.\n"
    "\n"
    "- [ ] fu-abc123 — harden the parser (p1)\n"
    "  source: PR#100\n"
    "  why: regex was fu-only\n"
    "- [ ] cv-deadbeef — out-of-scope bug in foo (p2)\n"
    "- [ ] ab-12345678 — **wire the thing** (p2). shipped via PR#100 "
    "source: PR#100 filed: ab-12345678\n"
    "- [ ] follow up with the design team #jc \U0001F4C5 2026-06-09\n"
)


def test_parse_managed_items_groups_four_types() -> None:
    """AC1: fu/cv/ab/#jc are each classified with id, priority, status, section."""
    from fno.backlog.capture import parse_managed_items

    items = parse_managed_items(_BY_TYPE_FIXTURE)
    by_type = {i["type"]: i for i in items}
    assert set(by_type) == {"followup", "carveout", "node", "human"}

    assert by_type["followup"]["id"] == "fu-abc123"
    assert by_type["followup"]["priority"] == "p1"
    assert by_type["followup"]["title"] == "harden the parser"

    assert by_type["carveout"]["id"] == "cv-deadbeef"
    assert by_type["carveout"]["priority"] == "p2"

    # The real post-merge shape: (pN) sits mid-line with prose after it.
    assert by_type["node"]["id"] == "ab-12345678"
    assert by_type["node"]["priority"] == "p2"
    assert by_type["node"]["title"] == "**wire the thing**"

    assert by_type["human"]["id"] is None
    assert "follow up with the design team" in by_type["human"]["title"]

    # Every item traces to the nearest preceding marker (the post-merge marker
    # sits between the date heading and the items, so it wins over the heading).
    assert all(i["section"] == "post-merge:pr-100" for i in items)
    assert all(i["status"] == "open" for i in items)


def test_parse_managed_items_ignores_prose_tokens() -> None:
    """AC2: fu-/ab- tokens mentioned in a narrative paragraph (no checkbox) are
    never reported as managed items."""
    from fno.backlog.capture import parse_managed_items

    items = parse_managed_items(_BY_TYPE_FIXTURE)
    ids = {i["id"] for i in items}
    assert "fu-bad999" not in ids
    assert "ab-feedface" not in ids
    # Exactly the four checkbox-line items, nothing from the prose.
    assert len(items) == 4


def test_parse_managed_items_excludes_struck_by_default() -> None:
    """Struck (promoted/dismissed) lines hide unless include_struck=True, the
    same default as parse_items."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## 2026-06-03\n"
        "- [ ] fu-abc123 — open one (p1)\n"
        "- [x] fu-def456 — promoted one (p2) -> ab-99887766\n"
        "- [-] cv-deadbeef — dismissed one (p3) (dismissed: nope)\n"
    )
    open_ids = {i["id"] for i in parse_managed_items(text)}
    assert open_ids == {"fu-abc123"}
    all_status = {
        i["id"]: i["status"]
        for i in parse_managed_items(text, include_struck=True)
    }
    assert all_status == {
        "fu-abc123": "open",
        "fu-def456": "promoted",
        "cv-deadbeef": "dismissed",
    }


def test_parse_managed_items_accepts_both_separators() -> None:
    """The lens reads the legacy em-dash AND the target hyphen separator (Phase 2
    migrates the writers; the reader must accept both from the start)."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## h\n"
        "- [ ] fu-abc123 — em dash (p1)\n"
        "- [ ] cv-deadbeef - hyphen (p2)\n"
    )
    got = {i["id"]: i["priority"] for i in parse_managed_items(text)}
    assert got == {"fu-abc123": "p1", "cv-deadbeef": "p2"}


def test_cli_list_by_type_json_four_buckets(tmp_path: Path) -> None:
    """AC1 (CLI): list --by-type --json emits four labeled buckets, each item
    carrying its priority + source section, and no prose token leaks in."""
    from fno.backlog.capture import cli, _inbox_path

    inbox = _inbox_path()
    inbox.parent.mkdir(parents=True, exist_ok=True)
    inbox.write_text(_BY_TYPE_FIXTURE, encoding="utf-8")

    res = runner.invoke(cli, ["list", "--by-type", "--json"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert set(payload) == {"your_actions", "followups", "carveouts", "filed_nodes"}

    assert [i["id"] for i in payload["followups"]] == ["fu-abc123"]
    assert [i["id"] for i in payload["carveouts"]] == ["cv-deadbeef"]
    assert [i["id"] for i in payload["filed_nodes"]] == ["ab-12345678"]
    assert payload["your_actions"][0]["id"] is None
    assert payload["followups"][0]["priority"] == "p1"
    assert payload["filed_nodes"][0]["section"] == "post-merge:pr-100"

    all_ids = {i["id"] for bucket in payload.values() for i in bucket}
    assert "fu-bad999" not in all_ids and "ab-feedface" not in all_ids


def test_cli_list_by_type_empty_inbox(tmp_path: Path) -> None:
    """An empty / absent inbox yields four empty buckets, never an error."""
    from fno.backlog.capture import cli

    res = runner.invoke(cli, ["list", "--by-type", "--json"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert payload == {
        "your_actions": [],
        "followups": [],
        "carveouts": [],
        "filed_nodes": [],
    }


def test_parse_managed_items_rejects_malformed_tokens() -> None:
    """The node family accepts the configurable 4-8 hex width (ab-bbfccb8f), so a
    token outside that band is malformed: below 4 hex or above 8 hex. cv- stays a
    strict 8-hex sibling and fu- accepts lower-kebab slugs but rejects uppercase /
    empty bodies. A wrong-shape token is not a managed item. Pins the grammar
    bounds so a future loosening cannot silently pass green."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## h\n"
        "- [ ] ab-123 — three hex, below the 4-hex floor (p2)\n"
        "- [ ] ab-123456789 — nine hex, above the 8-hex ceiling (p3)\n"
        "- [ ] cv-deadbee — seven hex cv, sibling stays strict 8 (p2)\n"
        "- [ ] fu-ABCDE — uppercase, not a lower-kebab slug (p1)\n"
        "- [ ] fu- — empty slug body (p3)\n"
    )
    assert parse_managed_items(text, include_struck=True) == []


def test_parse_managed_items_accepts_configured_width_node() -> None:
    """ab-bbfccb8f: a node token at the configurable 4-hex width is a valid filed
    node, recognized alongside legacy 8-hex ids."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## h\n"
        "- [ ] xy-a3f9 — four-hex configured node (p2)\n"
        "- [ ] ab-932f5a92 — legacy eight-hex node (p1)\n"
    )
    items = parse_managed_items(text, include_struck=True)
    assert {it["id"] for it in items} == {"xy-a3f9", "ab-932f5a92"}
    assert all(it["type"] == "node" for it in items)


def test_parse_recognizes_hand_authored_slug_fu_ids() -> None:
    """ab-932f5a92: hand-authored slug fu-ids (fu-cwd339, fu-codex-errpaths,
    fu-341flake) that violate the minted 6-hex grammar are still recognized by
    the checkbox-line parsers, so list --by-type / tidy / digest see them. A slug
    mentioned in narrative prose (no checkbox) stays excluded (AC2), and minting
    is unaffected - mint_fu_id still mints 6 lowercase hex."""
    import re

    from fno.backlog.capture import (
        bucket_managed_items,
        mint_fu_id,
        parse_items,
        parse_managed_items,
    )

    text = (
        "## 2026-06-03\n"
        "- [ ] fu-cwd339 - hand slug (p2)\n"
        "- [ ] fu-codex-errpaths - codex error paths (p1)\n"
        "- [ ] fu-341flake - flaky test (p3)\n"
        "- [ ] fu-a1b2c3 - a minted id (p2)\n"
        "\n"
        "Prose mentioning fu-codex-errpaths and fu-a1b2c3 with no checkbox.\n"
    )

    managed = parse_managed_items(text)
    ids = [it["id"] for it in managed]
    # all four (3 slugs + 1 minted) recognized as followups, in source order
    assert ids == ["fu-cwd339", "fu-codex-errpaths", "fu-341flake", "fu-a1b2c3"]
    assert all(it["type"] == "followup" for it in managed)
    assert bucket_managed_items(managed)["followups"] == managed
    # the prose mention does not leak as an item (AC2)
    assert ids.count("fu-codex-errpaths") == 1
    # the legacy fu-only parser sees them too
    assert [i["id"] for i in parse_items(text)] == ids
    # minting is unchanged: still 6 lowercase hex
    assert re.fullmatch(r"fu-[0-9a-f]{6}", mint_fu_id(set()))


def test_parse_managed_items_jc_priority_and_no_truncation() -> None:
    """A trailing (pN) on a #jc line IS its priority; a (pN) embedded in the
    human-readable text is NOT (it must not truncate the title)."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## h\n"
        "- [ ] ship the thing #jc (p1)\n"
        "- [ ] talk to (p2) team about budget #jc 2026-06-09\n"
    )
    items = parse_managed_items(text)
    assert [i["type"] for i in items] == ["human", "human"]
    assert items[0]["priority"] == "p1"
    assert items[0]["title"] == "ship the thing #jc"
    # The mid-line (p2) is prose, not a priority: title stays intact.
    assert items[1]["priority"] is None
    assert "talk to (p2) team about budget" in items[1]["title"]


def test_parse_managed_items_section_falls_back_to_heading() -> None:
    """With no post-merge marker present, the nearest preceding ## heading is
    the section."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## 2026-06-04 structural followups\n"
        "- [ ] fu-abc123 — under a bare heading (p2)\n"
    )
    items = parse_managed_items(text)
    assert len(items) == 1
    assert items[0]["section"] == "2026-06-04 structural followups"


def test_parse_managed_items_ab_hyphen_and_struck() -> None:
    """The ab- token (last in the alternation) parses with the hyphen separator,
    and a struck ab- line is hidden by default / surfaced with include_struck."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## h\n"
        "- [ ] ab-12345678 - hyphen-separated node (p2)\n"
        "- [x] ab-99887766 — filed thing (p1) -> ab-deadbeef\n"
    )
    open_items = parse_managed_items(text)
    assert [(i["id"], i["type"], i["priority"]) for i in open_items] == [
        ("ab-12345678", "node", "p2")
    ]
    all_items = parse_managed_items(text, include_struck=True)
    assert {i["id"]: i["status"] for i in all_items} == {
        "ab-12345678": "open",
        "ab-99887766": "promoted",
    }


def test_type_prefix_bucket_tables_in_sync() -> None:
    """Structural guard: every type a parsed item can carry has a bucket, so
    bucket_managed_items can never KeyError on real parser output."""
    from fno.backlog.capture import _BUCKET_BY_TYPE, _TYPE_BY_PREFIX

    assert set(_TYPE_BY_PREFIX.values()) | {"human"} == set(_BUCKET_BY_TYPE)


def test_parse_managed_items_fu_cv_preserve_embedded_priority_text() -> None:
    """fu-/cv- priority is the TRAILING (pN); a parenthetical embedded in the
    title must NOT be mis-read as the priority (codex P2 on PR #429). Only ab-
    filed-node lines use the lenient mid-line split."""
    from fno.backlog.capture import parse_managed_items

    text = (
        "## h\n"
        "- [ ] fu-abc123 — talk to (p2) team about budget (p1)\n"
        "- [ ] cv-deadbeef — fix the (p0) handler (p2)\n"
        "- [ ] ab-12345678 — **filed** (p3). prose source: PR#9\n"
    )
    items = parse_managed_items(text)
    assert (items[0]["title"], items[0]["priority"]) == (
        "talk to (p2) team about budget",
        "p1",
    )
    assert (items[1]["title"], items[1]["priority"]) == ("fix the (p0) handler", "p2")
    # ab- still uses the lenient first-(pN) split for the post-merge prose shape.
    assert (items[2]["title"], items[2]["priority"]) == ("**filed**", "p3")
