"""Tests for the retro postmortem source (W6 6.2, x-f063).

The AC that matters most is AC6-FR: interrupted/reran harvests never
re-propose an entry that already carries ``consumed_at``, and the stamp lands
only AFTER the disposition, so a mid-run crash re-processes exactly the
unstamped tail.
"""
from __future__ import annotations

from pathlib import Path

from fno.retro.classify import (
    DISPOSITION_ARCHIVE,
    DISPOSITION_INBOX,
    DISPOSITION_NODE,
    classify_postmortem,
)
from fno.retro.dedup import existing_keys_from_nodes, trailer
from fno.retro.harvest import harvest_postmortems, stamp_postmortem_consumed
from fno.retro.routine import triage_postmortems
from fno.retro.types import KIND_POSTMORTEM, RawItem

LEGACY = """---
type: target-postmortem
session_id: 20260529T205333Z-x
target_invocation: "/target ab-9dd70ad2"
blocked_reason:
  kind: user_cancel
  details: "user:sentinel"
---

# Postmortem: 20260529

## Last output of failed phase
cancelled by user
"""

FINALIZE_FMT = """# Postmortem: abc123

- session: `sid-1`
- termination: **NoProgress** (stuck: exited without shipping)
- node: `x-1`

## Last assistant message

the worker hit a split-brain: a respawned claim went stale while the session lived
"""


def _write(dirp: Path, name: str, text: str) -> Path:
    p = dirp / name
    p.write_text(text, encoding="utf-8")
    return p


# -- harvest ------------------------------------------------------------------

def test_harvest_reads_both_formats(tmp_path: Path) -> None:
    _write(tmp_path, "a-legacy.md", LEGACY)
    _write(tmp_path, "b-finalize.md", FINALIZE_FMT)
    items = harvest_postmortems(tmp_path)
    assert [i.source_id for i in items] == [
        "postmortem:a-legacy.md",
        "postmortem:b-finalize.md",
    ]
    legacy, fin = items
    assert legacy.kind == KIND_POSTMORTEM and legacy.subkind == "user_cancel"
    assert "/target ab-9dd70ad2" in (legacy.title_hint or "")
    assert fin.subkind == "NoProgress" and "split-brain" in fin.text
    # No target_invocation in the finalize format -> the unique filename stem
    # keeps same-reason entries from colliding in the inbox (where,title) dedup.
    assert "b-finalize" in (fin.title_hint or "")


def test_same_reason_postmortems_get_distinct_titles(tmp_path: Path) -> None:
    _write(tmp_path, "a.md", FINALIZE_FMT)
    _write(tmp_path, "b.md", FINALIZE_FMT)
    a, b = harvest_postmortems(tmp_path)
    assert a.title_hint != b.title_hint


def test_harvest_skips_consumed(tmp_path: Path) -> None:
    _write(
        tmp_path,
        "done.md",
        "---\nconsumed_at: 2026-07-01T00:00:00+00:00\n---\n# Postmortem\n",
    )
    assert harvest_postmortems(tmp_path) == []


def test_harvest_malformed_warns_not_aborts(tmp_path: Path) -> None:
    _write(tmp_path, "bad.md", "---\n: : bad yaml [\n---\nbody\n")
    _write(tmp_path, "good.md", FINALIZE_FMT)
    warnings: list[str] = []
    items = harvest_postmortems(tmp_path, warnings=warnings)
    assert len(items) == 1 and items[0].source_id == "postmortem:good.md"
    assert warnings and "bad.md" in warnings[0]


def test_harvest_gist_is_bounded(tmp_path: Path) -> None:
    _write(tmp_path, "big.md", "# Postmortem\n" + "line\n" * 500)
    (item,) = harvest_postmortems(tmp_path)
    assert len(item.text.splitlines()) <= 40


def test_harvest_missing_dir_is_empty(tmp_path: Path) -> None:
    assert harvest_postmortems(tmp_path / "nope") == []


# -- stamp --------------------------------------------------------------------

def test_stamp_prepends_frontmatter_when_absent(tmp_path: Path) -> None:
    p = _write(tmp_path, "pm.md", FINALIZE_FMT)
    stamp_postmortem_consumed(p, ts="2026-07-04T00:00:00+00:00")
    text = p.read_text()
    assert text.startswith("---\nconsumed_at: 2026-07-04T00:00:00+00:00\n---\n")
    assert "split-brain" in text  # body preserved
    assert harvest_postmortems(tmp_path) == []  # AC6-FR: not re-proposed


def test_stamp_inserts_into_existing_frontmatter(tmp_path: Path) -> None:
    p = _write(tmp_path, "pm.md", LEGACY)
    stamp_postmortem_consumed(p, ts="2026-07-04T00:00:00+00:00")
    text = p.read_text()
    assert text.startswith("---\n")
    assert "consumed_at: 2026-07-04T00:00:00+00:00" in text
    assert "type: target-postmortem" in text  # existing keys preserved
    assert harvest_postmortems(tmp_path) == []


def test_empty_frontmatter_harvests_and_stamps_cleanly(tmp_path: Path) -> None:
    """`---\\n---\\nbody` (empty fm) must not grow a second fm block on stamp."""
    p = _write(tmp_path, "pm.md", "---\n---\n# Postmortem\n\nsome body\n")
    (item,) = harvest_postmortems(tmp_path)
    assert item.source_id == "postmortem:pm.md"
    stamp_postmortem_consumed(p, ts="2026-07-04T00:00:00+00:00")
    text = p.read_text()
    assert text.startswith("---\nconsumed_at: 2026-07-04T00:00:00+00:00\n---\n")
    assert text.count("---\n") == 2 and "some body" in text
    assert harvest_postmortems(tmp_path) == []


def test_stamp_is_idempotent(tmp_path: Path) -> None:
    p = _write(tmp_path, "pm.md", FINALIZE_FMT)
    stamp_postmortem_consumed(p, ts="2026-07-04T00:00:00+00:00")
    before = p.read_text()
    stamp_postmortem_consumed(p, ts="2027-01-01T00:00:00+00:00")
    assert p.read_text() == before  # second stamp is a no-op


# -- classify -----------------------------------------------------------------

def _item(text: str, subkind: str | None = None) -> RawItem:
    return RawItem(
        kind=KIND_POSTMORTEM, text=text, source_id="postmortem:x.md", subkind=subkind
    )


def test_classify_cancel_is_archived() -> None:
    disposition, cand = classify_postmortem(_item("whatever", subkind="user_cancel"))
    assert disposition == DISPOSITION_ARCHIVE and cand is None


def test_classify_wedge_text_is_node() -> None:
    disposition, cand = classify_postmortem(
        _item("worker hit a split-brain after respawn", subkind="NoProgress")
    )
    assert disposition == DISPOSITION_NODE
    assert cand is not None and cand.tier == "node" and cand.priority == "p2"
    assert cand.source_pr is None and not cand.uncited
    assert "Source: postmortem:x.md" in cand.body


def test_classify_ambiguous_is_inbox_not_guessed() -> None:
    disposition, cand = classify_postmortem(
        _item("session ran out of budget doing normal work", subkind="Budget")
    )
    assert disposition == DISPOSITION_INBOX and cand is not None and cand.tier == "inbox"


def test_classify_interrupted_wedge_is_node_not_archived() -> None:
    """x-42f6 regression: a finalize-format Interrupted/Aborted postmortem must
    classify by body, NOT auto-archive on the interrupt/abort substring - else
    the widened stuck-session corpus is silently dropped (codex P2)."""
    for reason in ("Interrupted", "Aborted"):
        disposition, cand = classify_postmortem(
            _item("worker hit a split-brain after respawn", subkind=reason)
        )
        assert disposition == DISPOSITION_NODE, f"{reason} wedge must be a node"
        assert cand is not None and cand.tier == "node"


def test_classify_interrupted_benign_is_inbox_not_archived() -> None:
    """A stuck-reason postmortem with no wedge text surfaces to inbox (not
    archived, not guessed into a node)."""
    for reason in ("Interrupted", "Aborted"):
        disposition, cand = classify_postmortem(
            _item("session was cancelled while doing normal work", subkind=reason)
        )
        assert disposition == DISPOSITION_INBOX, f"{reason} no-wedge must be inbox"
        assert cand is not None and cand.tier == "inbox"


def test_classify_reasonless_cancel_wording_is_not_archived() -> None:
    """Archive requires an EXPLICIT one-off reason kind: a reason-less gist
    merely quoting cancel-ish words (.target-cancelled sentinel) is ambiguous
    and must surface, not be silently consumed."""
    disposition, cand = classify_postmortem(
        _item("worker saw .fno/.target-cancelled and stopped", subkind=None)
    )
    assert disposition == DISPOSITION_INBOX and cand is not None


# -- dedup round-trip (degraded stamp path) ------------------------------------

def test_none_pr_trailer_round_trips() -> None:
    t = trailer(None, "ab12cd34ef56ab12")
    keys = existing_keys_from_nodes([{"id": "x-1", "details": f"body\n\n{t}"}])
    assert keys == {"None:ab12cd34ef56ab12"}


# -- triage_postmortems (the full drain) ---------------------------------------

def _fake_create(created: list):
    def create_fn(*, title, details, priority, project, cwd, domain="code", queued=False, **kw):
        created.append({"title": title, "details": details, "queued": queued})
        return f"x-new{len(created)}"

    return create_fn


def test_triage_drains_each_entry_to_one_disposition(tmp_path: Path) -> None:
    _write(tmp_path, "a-cancel.md", LEGACY)  # archive
    _write(tmp_path, "b-wedge.md", FINALIZE_FMT)  # node
    _write(
        tmp_path,
        "c-budget.md",
        "# Postmortem\n\n- termination: **Budget**\n\nplain overspend\n",
    )  # inbox
    created: list = []
    inboxed: list = []
    report = triage_postmortems(
        postmortems_dir=tmp_path,
        repo_root=tmp_path,
        existing_nodes=[],
        create_fn=_fake_create(created),
        inbox_fn=lambda c: inboxed.append(c),
    )
    assert report.harvested == 3
    assert report.dispositions == {
        "postmortem:a-cancel.md": "archived",
        "postmortem:b-wedge.md": "node",
        "postmortem:c-budget.md": "inbox",
    }
    assert len(created) == 1 and created[0]["queued"] is True  # interactive default
    assert len(inboxed) == 1
    assert report.stamped == 3  # every dispositioned entry consumed

    # AC6-FR: rerun proposes nothing and creates nothing.
    rerun = triage_postmortems(
        postmortems_dir=tmp_path,
        repo_root=tmp_path,
        existing_nodes=[],
        create_fn=_fake_create(created),
        inbox_fn=lambda c: inboxed.append(c),
    )
    assert rerun.harvested == 0 and len(created) == 1 and len(inboxed) == 1


def test_triage_failed_land_leaves_entry_unstamped(tmp_path: Path) -> None:
    _write(tmp_path, "wedge.md", FINALIZE_FMT)

    def boom(**kw):
        raise RuntimeError("graph locked")

    report = triage_postmortems(
        postmortems_dir=tmp_path,
        repo_root=tmp_path,
        existing_nodes=[],
        create_fn=boom,
        inbox_fn=lambda c: None,
    )
    assert report.dispositions == {"postmortem:wedge.md": "failed"}
    assert report.stamped == 0 and report.failed
    # The unstamped tail re-processes next run.
    assert len(harvest_postmortems(tmp_path)) == 1


# -- CLI wiring (`fno retro run`) ----------------------------------------------

def _wire_cli(tmp_path: Path, monkeypatch):
    import fno.paths as _paths
    import fno.retro.cli as _cli

    pm_dir = tmp_path / "postmortems"
    pm_dir.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setattr(_paths, "retro_pending_dir", lambda: tmp_path / "no-sentinels")
    monkeypatch.setattr(_paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(_paths, "postmortems_dir", lambda: pm_dir)
    monkeypatch.setattr(_cli, "_current_repo_slug", lambda *a, **k: None)
    return pm_dir


def test_plain_run_drains_postmortems(tmp_path: Path, monkeypatch) -> None:
    from typer.testing import CliRunner

    from fno.cli import app

    pm_dir = _wire_cli(tmp_path, monkeypatch)
    _write(pm_dir, "cancel.md", LEGACY)
    res = CliRunner().invoke(app, ["retro", "run"])
    assert res.exit_code == 0, res.output
    assert "1 archived" in res.output
    assert "consumed_at" in (pm_dir / "cancel.md").read_text()


# -- CLI wiring (`fno retro drain-postmortems`, x-42f6 US3) ---------------------

def test_drain_postmortems_drains_and_reports(tmp_path: Path, monkeypatch) -> None:
    """AC4-HP: the narrow verb drains each postmortem, stamps consumed_at, and
    exits 0. AC4-FR: a second drain is an idempotent no-op."""
    from typer.testing import CliRunner

    from fno.cli import app

    pm_dir = _wire_cli(tmp_path, monkeypatch)
    _write(pm_dir, "cancel.md", LEGACY)
    res = CliRunner().invoke(app, ["retro", "drain-postmortems"])
    assert res.exit_code == 0, res.output
    assert "1 archived" in res.output
    assert "consumed_at" in (pm_dir / "cancel.md").read_text()

    # AC4-FR: re-drain proposes nothing (consumed_at guard) -> zero-count line.
    rerun = CliRunner().invoke(app, ["retro", "drain-postmortems"])
    assert rerun.exit_code == 0, rerun.output
    assert "0 unconsumed" in rerun.output


def test_drain_postmortems_empty_is_zero_count(tmp_path: Path, monkeypatch) -> None:
    """AC4-EDGE: no postmortems -> a zero-count line and exit 0."""
    from typer.testing import CliRunner

    from fno.cli import app

    _wire_cli(tmp_path, monkeypatch)  # empty postmortems dir
    res = CliRunner().invoke(app, ["retro", "drain-postmortems"])
    assert res.exit_code == 0, res.output
    assert "0 unconsumed" in res.output


def test_targeted_run_skips_postmortems(tmp_path: Path, monkeypatch) -> None:
    """--node/--pr-number is a targeted harvest; the global postmortem drain
    only rides the plain run."""
    from typer.testing import CliRunner

    from fno.cli import app

    pm_dir = _wire_cli(tmp_path, monkeypatch)
    _write(pm_dir, "cancel.md", LEGACY)
    res = CliRunner().invoke(app, ["retro", "run", "--node", "x-none"])
    assert res.exit_code == 0, res.output
    assert "consumed_at" not in (pm_dir / "cancel.md").read_text()


def test_triage_stamp_failure_is_degraded_and_dedups_on_rerun(tmp_path: Path) -> None:
    _write(tmp_path, "wedge.md", FINALIZE_FMT)
    created: list = []

    def failing_stamp(path: Path) -> None:
        raise OSError("read-only fs")

    report = triage_postmortems(
        postmortems_dir=tmp_path,
        repo_root=tmp_path,
        existing_nodes=[],
        create_fn=_fake_create(created),
        inbox_fn=lambda c: None,
        stamp_fn=failing_stamp,
    )
    assert report.dispositions == {"postmortem:wedge.md": "node"}
    assert report.stamped == 0 and report.warnings and len(created) == 1

    # Rerun with the node (carrying the land trailer) live in the graph:
    # dedup collapses the re-proposal to a consume, no second node.
    node = {"id": "x-new1", "details": created[0]["details"]}
    rerun = triage_postmortems(
        postmortems_dir=tmp_path,
        repo_root=tmp_path,
        existing_nodes=[node],
        create_fn=_fake_create(created),
        inbox_fn=lambda c: None,
    )
    assert rerun.dispositions == {"postmortem:wedge.md": "dupe"}
    assert len(created) == 1  # no duplicate node
    assert rerun.stamped == 1  # this time the stamp lands
    assert harvest_postmortems(tmp_path) == []
