"""Integration tests for `fno carveout add` (Wave 2.1, US1).

Covers AC1-HP / AC1-ERR / AC1-UI / AC1-EDGE / AC1-FR from the retro-auto-triage
design. Capture must never be silently lost: a missing session degrades to an
unscoped record (exit 0 + warn), while a FAILED write exits non-zero.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _read_carveouts(repo_root: Path) -> list[dict]:
    f = repo_root / ".fno" / "carveouts.jsonl"
    assert f.exists(), f"expected carveouts ledger at {f}"
    return [json.loads(ln) for ln in f.read_text(encoding="utf-8").splitlines() if ln.strip()]


def _write_state(repo_root: Path, session_id: str) -> None:
    (repo_root / ".fno").mkdir(parents=True, exist_ok=True)
    (repo_root / ".fno" / "target-state.md").write_text(
        f"---\nstatus: IN_PROGRESS\nsession_id: {session_id}\n---\nbody\n",
        encoding="utf-8",
    )


@pytest.fixture(autouse=True)
def _repo(tmp_path: Path, monkeypatch):
    """Pin tmp_path as repo root and clear any inherited session env."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return tmp_path


def test_ac1_hp_active_session(_repo: Path):
    """AC1-HP: scoped record with all fields; id to stdout; exit 0."""
    sid = "20260524T120000Z-11111-abcdef"
    _write_state(_repo, sid)
    result = runner.invoke(
        app,
        ["carveout", "add", "--kind", "deferred", "--need", "which auth provider",
         "skipped SSO wiring pending provider choice"],
    )
    assert result.exit_code == 0, result.output
    entries = _read_carveouts(_repo)
    assert len(entries) == 1
    e = entries[0]
    assert e["kind"] == "deferred"
    assert e["need"] == "which auth provider"
    assert e["description"] == "skipped SSO wiring pending provider choice"
    assert e["session_id"] == sid
    assert e["ts"].endswith("Z")
    assert e["id"].startswith("cv-")
    # The new id is the value on stdout.
    assert e["id"] in result.stdout


def test_ac1_err_missing_kind(_repo: Path):
    """AC1-ERR: missing --kind -> exit 2, nothing appended."""
    result = runner.invoke(app, ["carveout", "add", "some description"])
    assert result.exit_code == 2, result.output
    assert not (_repo / ".fno" / "carveouts.jsonl").exists()


def test_ac1_err_invalid_kind(_repo: Path):
    """AC1-ERR (variant): an unknown --kind value -> exit 2, nothing appended."""
    result = runner.invoke(app, ["carveout", "add", "--kind", "bogus", "desc"])
    assert result.exit_code == 2
    assert not (_repo / ".fno" / "carveouts.jsonl").exists()


def test_ac1_err_invalid_priority(_repo: Path):
    """Invalid --priority -> exit 2 before any write."""
    result = runner.invoke(
        app, ["carveout", "add", "--kind", "oos-bug", "--priority", "high", "desc"]
    )
    assert result.exit_code == 2
    assert not (_repo / ".fno" / "carveouts.jsonl").exists()


def test_ac1_ui_no_session(_repo: Path):
    """AC1-UI: no resolvable session -> unscoped record, stderr warn, exit 0."""
    # No target-state.md and no CLAUDECODE_SESSION_ID (cleared in fixture).
    result = runner.invoke(app, ["carveout", "add", "--kind", "oos-bug", "found a leak"])
    assert result.exit_code == 0, result.output
    entries = _read_carveouts(_repo)
    assert len(entries) == 1
    assert entries[0]["session_id"] is None
    # CliRunner merges stderr into output by default; the warning must surface.
    assert "no active session" in result.output


def test_ac1_edge_oversize_truncated(_repo: Path):
    """AC1-EDGE: a 50KB description is truncated to the cap with a marker, still appended."""
    from fno.carveout.core import DESCRIPTION_CAP

    big = "x" * 50_000
    result = runner.invoke(app, ["carveout", "add", "--kind", "deferred", big])
    assert result.exit_code == 0, result.output
    entries = _read_carveouts(_repo)
    assert len(entries) == 1
    e = entries[0]
    assert e["truncated"] is True
    assert "[truncated" in e["description"]
    # Body retained is the cap plus the marker; well under the original 50KB.
    assert len(e["description"]) < 50_000
    assert e["description"].startswith("x" * DESCRIPTION_CAP)


def test_consume_carveouts_removes_processed_ids(_repo: Path):
    """consume_carveouts removes triaged ids so they are never re-filed; keeps the rest."""
    from fno.carveout.core import consume_carveouts

    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":"cv-1","kind":"deferred","description":"a"}\n'
        "garbage line\n"
        '{"id":"cv-2","kind":"oos-bug","description":"b"}\n',
        encoding="utf-8",
    )
    removed = consume_carveouts(_repo, ["cv-1"])
    assert removed == 1
    remaining = ledger.read_text(encoding="utf-8")
    assert "cv-1" not in remaining
    assert "cv-2" in remaining
    assert "garbage line" in remaining  # malformed line preserved, not dropped


def test_ac1_fr_unwritable_ledger(_repo: Path):
    """AC1-FR: an unwritable ledger -> non-zero exit + stderr (no silent success)."""
    # Make the ledger path a directory so the append open() raises OSError.
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.mkdir()
    result = runner.invoke(app, ["carveout", "add", "--kind", "deferred", "desc"])
    assert result.exit_code != 0
    assert "failed to record" in result.output


# -- ab-44408b6e: carve-outs are durable across the worktree boundary --

def test_resolve_carveout_root_uses_canonical(monkeypatch):
    """resolve_carveout_root delegates to the CANONICAL repo root, never the
    per-worktree --show-toplevel root (so the ledger survives worktree
    archival)."""
    import fno.paths as _paths
    from fno.carveout.core import resolve_carveout_root

    monkeypatch.setattr(_paths, "resolve_canonical_repo_root", lambda: Path("/canon"))
    assert resolve_carveout_root() == Path("/canon")


def test_add_carveout_storage_root_splits_session_and_ledger(tmp_path: Path, monkeypatch):
    """Session id is read from repo_root (the worktree); the ledger is written
    under storage_root (canonical). The two roots are independent."""
    from fno.carveout.core import add_carveout

    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    session_root = tmp_path / "wt"
    (session_root / ".fno").mkdir(parents=True)
    (session_root / ".fno" / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\nsession_id: wt-sess\n---\nbody\n", encoding="utf-8"
    )
    storage_root = tmp_path / "canon"

    cv, unscoped = add_carveout(
        session_root, kind="deferred", description="d", storage_root=storage_root
    )
    assert unscoped is False
    assert cv.session_id == "wt-sess"  # resolved from the worktree's state
    assert (storage_root / ".fno" / "carveouts.jsonl").exists()  # canonical ledger
    assert not (session_root / ".fno" / "carveouts.jsonl").exists()


def test_cli_ledger_climbs_to_canonical_root(tmp_path: Path, monkeypatch):
    """End-to-end: `fno carveout add` from a linked worktree writes the ledger
    under canonical while reading the session from the worktree (ab-44408b6e)."""
    import fno.paths as _paths

    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    (worktree / ".fno").mkdir(parents=True)
    (canonical / ".fno").mkdir(parents=True)
    (worktree / ".fno" / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\nsession_id: wt-sess\n---\nbody\n", encoding="utf-8"
    )
    monkeypatch.setattr(_paths, "resolve_repo_root", lambda: worktree)
    monkeypatch.setattr(_paths, "resolve_canonical_repo_root", lambda: canonical)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)

    result = runner.invoke(app, ["carveout", "add", "--kind", "deferred", "x"])
    assert result.exit_code == 0, result.output

    canon_ledger = canonical / ".fno" / "carveouts.jsonl"
    assert canon_ledger.exists()
    assert not (worktree / ".fno" / "carveouts.jsonl").exists()
    entry = json.loads(canon_ledger.read_text(encoding="utf-8").splitlines()[0])
    assert entry["session_id"] == "wt-sess"


# -- ab-4a1a4fea Group 3: backfill carve-out kind + list/resolve surface --
#
# A `backfill` carve-out declares a data backfill the merged PR enables; --need
# carries its PRECONDITION (not an open question). /pr merged's backfill slot
# reads surviving backfill entries (`list`) and removes handled ones (`resolve`).


def test_backfill_kind_accepted(_repo: Path):
    """A `backfill` carve-out records with kind=backfill; --need carries the precondition."""
    sid = "20260608T000000Z-22222-bbbbbb"
    _write_state(_repo, sid)
    result = runner.invoke(
        app,
        ["carveout", "add", "--kind", "backfill",
         "--need", "migration 0042 applied",
         "backfill facility_timezone for pre-PR rows; run scripts/backfill_tz.py"],
    )
    assert result.exit_code == 0, result.output
    entries = _read_carveouts(_repo)
    assert len(entries) == 1
    e = entries[0]
    assert e["kind"] == "backfill"
    assert e["need"] == "migration 0042 applied"
    assert e["session_id"] == sid
    assert e["id"] in result.stdout


def test_carveout_list_filters_by_kind_json(_repo: Path):
    """`fno carveout list --kind backfill --json` emits ONLY backfill rows as JSONL."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":"cv-d","kind":"deferred","need":"q","description":"a deferred thing"}\n'
        '{"id":"cv-b","kind":"backfill","need":"mig X","description":"backfill cmd"}\n'
        '{"id":"cv-o","kind":"oos-bug","description":"a bug"}\n',
        encoding="utf-8",
    )
    result = runner.invoke(app, ["carveout", "list", "--kind", "backfill", "--json"])
    assert result.exit_code == 0, result.output
    rows = [json.loads(ln) for ln in result.stdout.splitlines() if ln.strip().startswith("{")]
    assert [r["id"] for r in rows] == ["cv-b"]
    assert rows[0]["kind"] == "backfill"


def test_carveout_list_all_without_kind(_repo: Path):
    """No --kind lists every carve-out (JSONL)."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":"cv-d","kind":"deferred","description":"a"}\n'
        '{"id":"cv-b","kind":"backfill","description":"b"}\n',
        encoding="utf-8",
    )
    result = runner.invoke(app, ["carveout", "list", "--json"])
    assert result.exit_code == 0, result.output
    rows = [json.loads(ln) for ln in result.stdout.splitlines() if ln.strip().startswith("{")]
    assert {r["id"] for r in rows} == {"cv-d", "cv-b"}


def test_carveout_list_empty_ledger_exit_zero(_repo: Path):
    """A missing ledger is the common case, NOT an error: clean exit 0, zero rows."""
    result = runner.invoke(app, ["carveout", "list", "--kind", "backfill", "--json"])
    assert result.exit_code == 0, result.output
    assert not [ln for ln in result.stdout.splitlines() if ln.strip().startswith("{")]


def test_carveout_list_skips_malformed_lines(_repo: Path):
    """A malformed ledger line is skipped, never aborts the listing (capture is never lost)."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":"cv-b","kind":"backfill","description":"b"}\n'
        "not json at all\n",
        encoding="utf-8",
    )
    result = runner.invoke(app, ["carveout", "list", "--kind", "backfill", "--json"])
    assert result.exit_code == 0, result.output
    rows = [json.loads(ln) for ln in result.stdout.splitlines() if ln.strip().startswith("{")]
    assert [r["id"] for r in rows] == ["cv-b"]


def test_carveout_list_unreadable_ledger_fails_loud(_repo: Path):
    """A present-but-unreadable ledger is a FAILED read, NOT "no carve-outs":
    exit non-zero + stderr (never a silent empty that drops a real backfill)."""
    # Make the ledger PATH a directory: it exists() but read_text raises OSError.
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.mkdir()
    result = runner.invoke(app, ["carveout", "list", "--kind", "backfill", "--json"])
    assert result.exit_code != 0
    assert "failed to read" in result.output


def test_carveout_resolve_removes_handled_ids(_repo: Path):
    """`fno carveout resolve <id>` removes handled backfill entries so a re-offer never repeats them."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":"cv-b","kind":"backfill","description":"b"}\n'
        '{"id":"cv-keep","kind":"backfill","description":"k"}\n',
        encoding="utf-8",
    )
    result = runner.invoke(app, ["carveout", "resolve", "cv-b"])
    assert result.exit_code == 0, result.output
    assert "resolved 1 carve-out" in result.stdout
    remaining = {
        json.loads(ln)["id"]
        for ln in ledger.read_text(encoding="utf-8").splitlines()
        if ln.strip()
    }
    assert remaining == {"cv-keep"}


def test_carveout_resolve_shortfall_warns(_repo: Path):
    """resolve fewer than requested (id absent or ledger unwritable) surfaces a
    stderr shortfall so a failed resolve is never mistaken for success."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text('{"id":"cv-b","kind":"backfill","description":"b"}\n', encoding="utf-8")
    # Request two ids; only cv-b exists -> removed (1) < requested (2).
    result = runner.invoke(app, ["carveout", "resolve", "cv-b", "cv-gone"])
    assert result.exit_code == 0, result.output
    assert "resolved 1 of 2 requested" in result.output
    assert "resolved 1 carve-out" in result.stdout


def test_carveout_resolve_dedupes_no_false_shortfall(_repo: Path):
    """A duplicated id must not inflate the requested count into a false shortfall
    (consume_carveouts dedupes internally)."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text('{"id":"cv-b","kind":"backfill","description":"b"}\n', encoding="utf-8")
    result = runner.invoke(app, ["carveout", "resolve", "cv-b", "cv-b"])
    assert result.exit_code == 0, result.output
    assert "resolved 1 carve-out" in result.stdout
    assert "requested id(s)" not in result.output  # no false shortfall warning


def test_carveout_list_filters_by_session(_repo: Path):
    """--session-id scopes the listing so /pr merged only sees ITS PR's backfills."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":"cv-a","kind":"backfill","session_id":"S1","description":"mine"}\n'
        '{"id":"cv-b","kind":"backfill","session_id":"S2","description":"theirs"}\n',
        encoding="utf-8",
    )
    result = runner.invoke(
        app, ["carveout", "list", "--kind", "backfill", "--session-id", "S1", "--json"]
    )
    assert result.exit_code == 0, result.output
    rows = [json.loads(ln) for ln in result.stdout.splitlines() if ln.strip().startswith("{")]
    assert [r["id"] for r in rows] == ["cv-a"]  # S2's backfill excluded


# -- `carveout list --pr-number` ledger join (x-f47f US2) --


def _pin_ledger(monkeypatch, path: Path, slug="bllshttng/footnote") -> None:
    import fno.graph._reconcile as R
    import fno.paths as P
    monkeypatch.setattr(P, "ledger_json", lambda: path)
    monkeypatch.setattr(R, "resolve_current_repo_slug", lambda *a, **k: slug)


def _write_backfills(repo: Path) -> None:
    ledger = repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":"cv-a","kind":"backfill","session_id":"S1","description":"mine"}\n'
        '{"id":"cv-b","kind":"backfill","session_id":"S2","description":"also mine"}\n'
        '{"id":"cv-c","kind":"backfill","session_id":"S9","description":"theirs"}\n',
        encoding="utf-8",
    )


def test_list_pr_number_resolves_both_sessions(_repo: Path, monkeypatch):
    """AC2-HP: a PR-480-shaped entry resolves BOTH session ids, and the listed
    carve-outs are exactly theirs - not the whole ledger."""
    lj = _repo / "ledger.json"
    lj.write_text(json.dumps({"entries": [
        {"pr_number": 480, "sessions": ["S1"], "session_id": "S2",
         "pr_url": "https://github.com/bllshttng/footnote/pull/480"},
        {"pr_number": 999, "session_id": "S9"},
    ]}), encoding="utf-8")
    _pin_ledger(monkeypatch, lj)
    _write_backfills(_repo)

    result = runner.invoke(
        app, ["carveout", "list", "--kind", "backfill", "--pr-number", "480", "--json"]
    )
    assert result.exit_code == 0, result.output
    out = json.loads(result.stdout.strip())
    assert sorted(out["sessions_resolved"]) == ["S1", "S2"]
    assert out["reason"] is None and out["consumable"] is True
    assert [c["id"] for c in out["carveouts"]] == ["cv-a", "cv-b"]


def test_list_pr_number_matches_pr_url_suffix(_repo: Path, monkeypatch):
    """An entry with no pr_number field still matches on its pr_url suffix."""
    lj = _repo / "ledger.json"
    lj.write_text(json.dumps({"entries": [
        {"pr_url": "https://github.com/bllshttng/footnote/pull/480", "session_id": "S1"},
        # Same number, different repo: must NOT be attributed to this PR.
        {"pr_url": "https://github.com/bllshttng/abilities/pull/480", "session_id": "SX"},
    ]}), encoding="utf-8")
    _pin_ledger(monkeypatch, lj)
    _write_backfills(_repo)

    result = runner.invoke(
        app, ["carveout", "list", "--kind", "backfill", "--pr-number", "480", "--json"]
    )
    out = json.loads(result.stdout.strip())
    assert out["sessions_resolved"] == ["S1"]


def test_list_pr_number_no_entry_states_reason_and_lists_readonly(_repo: Path, monkeypatch):
    """AC1-FR: no ledger entry -> the reason is reported, every backfill is listed
    read-only, and nothing is marked consumable."""
    lj = _repo / "ledger.json"
    lj.write_text(json.dumps({"entries": []}), encoding="utf-8")
    _pin_ledger(monkeypatch, lj)
    _write_backfills(_repo)

    result = runner.invoke(
        app, ["carveout", "list", "--kind", "backfill", "--pr-number", "480", "--json"]
    )
    assert result.exit_code == 0, result.output
    out = json.loads(result.stdout.strip())
    assert out["sessions_resolved"] == [] and out["consumable"] is False
    assert out["reason"] == "no ledger entry for PR #480"
    assert [c["id"] for c in out["carveouts"]] == ["cv-a", "cv-b", "cv-c"]


def test_list_pr_number_unreadable_ledger_is_not_no_match(_repo: Path, monkeypatch):
    """The bug this verb replaces: a broken read must not read as 'no owning
    session'. The reason distinguishes them."""
    lj = _repo / "ledger.json"
    lj.write_text("{not json", encoding="utf-8")
    _pin_ledger(monkeypatch, lj)
    _write_backfills(_repo)

    out = json.loads(runner.invoke(
        app, ["carveout", "list", "--kind", "backfill", "--pr-number", "480", "--json"]
    ).stdout.strip())
    assert out["consumable"] is False
    assert "unreadable" in out["reason"]


def test_list_pr_number_unresolvable_repo_refuses_to_attribute(_repo: Path, monkeypatch):
    """No repo slug -> PR numbers collide across repos, so ownership is refused."""
    lj = _repo / "ledger.json"
    lj.write_text(json.dumps({"entries": [{"pr_number": 480, "session_id": "S1"}]}),
                  encoding="utf-8")
    _pin_ledger(monkeypatch, lj, slug=None)
    _write_backfills(_repo)

    out = json.loads(runner.invoke(
        app, ["carveout", "list", "--kind", "backfill", "--pr-number", "480", "--json"]
    ).stdout.strip())
    assert out["sessions_resolved"] == [] and out["consumable"] is False
    assert "repo slug unresolved" in out["reason"]


def test_list_pr_number_refuses_url_less_ledger_rows(_repo: Path, monkeypatch):
    """A url-less ledger row carries no repo, and the ledger is GLOBAL - matching
    it on the bare number could claim a foreign PR's sessions and then CONSUME
    (destroy) their backfills. The consuming caller must refuse."""
    lj = _repo / "ledger.json"
    lj.write_text(json.dumps({"entries": [{"pr_number": 480, "session_id": "S1"}]}),
                  encoding="utf-8")
    _pin_ledger(monkeypatch, lj)
    _write_backfills(_repo)

    out = json.loads(runner.invoke(
        app, ["carveout", "list", "--kind", "backfill", "--pr-number", "480", "--json"]
    ).stdout.strip())
    assert out["sessions_resolved"] == [] and out["consumable"] is False


def test_resolve_pr_sessions_url_less_row_attributes_nothing(_repo: Path):
    """A url-less row names no repo, so it resolves no ownership for ANY caller.

    There is no opt-in: a per-caller trust flag would make correctness depend on
    every future caller reasoning right about a GLOBAL ledger, and one that
    guessed wrong would silently consume a same-numbered foreign PR's carve-outs.
    """
    from fno.ledger_join import resolve_pr_sessions

    lj = _repo / "ledger.json"
    lj.write_text(json.dumps({"entries": [{"pr_number": 480, "session_id": "S1"}]}),
                  encoding="utf-8")

    sessions, reason = resolve_pr_sessions(lj, 480, "bllshttng/footnote")
    assert sessions == [] and reason == "no ledger entry for PR #480"


def test_list_rejects_pr_number_with_session_id(_repo: Path):
    result = runner.invoke(
        app, ["carveout", "list", "--pr-number", "480", "--session-id", "S1"]
    )
    assert result.exit_code == 2
    assert "not both" in result.output


def test_carveout_list_null_values_render_placeholders(_repo: Path):
    """Explicit JSON null fields render as placeholders, never the string 'None'."""
    ledger = _repo / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        '{"id":null,"kind":null,"description":null,"need":null}\n', encoding="utf-8"
    )
    result = runner.invoke(app, ["carveout", "list"])  # human summary path
    assert result.exit_code == 0, result.output
    assert "None" not in result.stdout
    assert "? [?]" in result.stdout
