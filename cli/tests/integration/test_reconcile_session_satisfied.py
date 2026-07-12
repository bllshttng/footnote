"""Group 1 / Task 1.1 (ab-f7f8bc53): reconcile emits session_satisfied.

When `fno backlog reconcile` closes a backlog node whose PR merged outside the
ship gate, it must also emit a `session_satisfied{source:"pr_merge"}` event for
the OWNING target session so that session's stop hook can take the auto-complete
path instead of hard re-blocking. Today only an in-gate merge through
pr-merge.sh emits this; an out-of-band merge leaves the session hot.

Definition reused from the design doc: the event binds to the owning session via
session_id + gate_state_hash (md5 of the owning target-state.md at emit time), so
the stop hook's staleness check (check_session_satisfied) can match it.

Covers AC1-HP and AC1-ERR.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph import _reconcile as rec
from fno.graph._reconcile import (
    MergeDriftRecord,
    PrMergeState,
    emit_session_satisfied_for_record,
    scan_merge_drift,
)

runner = CliRunner()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _md5(path: Path) -> str:
    return hashlib.md5(path.read_bytes()).hexdigest()


def _write_state(cwd: Path, *, session_id: str, status: str = "IN_PROGRESS", pr_number: int = 42) -> Path:
    abil = cwd / ".fno"
    abil.mkdir(parents=True, exist_ok=True)
    state = abil / "target-state.md"
    state.write_text(
        "---\n"
        f"status: {status}\n"
        "current_phase: external\n"
        f"session_id: {session_id}\n"
        f"pr_number: {pr_number}\n"
        "---\n",
        encoding="utf-8",
    )
    return state


def _events(cwd: Path) -> list[dict]:
    f = cwd / ".fno" / "events.jsonl"
    if not f.exists():
        return []
    return [json.loads(line) for line in f.read_text().splitlines() if line.strip()]


def _record(cwd: Path | None, *, session_id: str | None = "sid-1", pr_number: int = 42) -> MergeDriftRecord:
    return MergeDriftRecord(
        node_id="ab-own",
        plan_path=None,
        pr_number=pr_number,
        pr_url=f"https://github.com/o/r/pull/{pr_number}",
        pr_state="MERGED",
        merged_at="2026-06-02T00:00:00Z",
        session_id=session_id,
        cwd=str(cwd) if cwd is not None else None,
    )


# ---------------------------------------------------------------------------
# scan_merge_drift threads session_id + cwd onto the record
# ---------------------------------------------------------------------------

def test_scan_threads_session_id_and_cwd_onto_record(tmp_path):
    # Live cwd: the forward-path dead-cwd guard (x-4114) degrades a gone cwd to
    # None, so a threading test must anchor on a real dir to see it pass through.
    live = str(tmp_path)
    entries = [{
        "id": "ab-own", "title": "t", "pr_number": 42,
        "pr_url": "https://github.com/test-owner/test-repo/pull/42",
        "additional_prs": [], "completed_at": None, "superseded_by": None,
        "plan_path": None, "cwd": live, "session_id": "sid-xyz",
    }]
    records = scan_merge_drift(entries, query=lambda n, repo=None, cwd=None: PrMergeState(
        number=n, state="MERGED", url="u", merged_at="2026-06-02T00:00:00Z"))
    assert len(records) == 1
    assert records[0].session_id == "sid-xyz"
    assert records[0].cwd == live


# ---------------------------------------------------------------------------
# emit_session_satisfied_for_record - pure function
# ---------------------------------------------------------------------------

def test_emit_writes_session_satisfied_for_owning_session(tmp_path):
    cwd = tmp_path / "repo"
    state = _write_state(cwd, session_id="sid-1")
    expected_hash = _md5(state)

    out = emit_session_satisfied_for_record(_record(cwd, session_id="sid-1"))
    assert out is not None
    evs = _events(cwd)
    ss = [e for e in evs if e["type"] == "session_satisfied"]
    assert len(ss) == 1
    data = ss[0]["data"]
    assert data["source"] == "pr_merge"
    assert data["reason"] == "reconcile_detected_merge"
    assert data["session_id"] == "sid-1"
    assert data["gate_state_hash"] == expected_hash
    assert data["evidence_url"] == "https://github.com/o/r/pull/42"
    assert ss[0]["source"] == "backlog"  # envelope producer identity


def test_emit_noop_when_no_state_file(tmp_path):
    cwd = tmp_path / "repo"  # no .fno/target-state.md
    out = emit_session_satisfied_for_record(_record(cwd))
    assert out is None
    assert _events(cwd) == []


def test_emit_noop_when_session_already_complete(tmp_path):
    cwd = tmp_path / "repo"
    _write_state(cwd, session_id="sid-1", status="COMPLETE")
    out = emit_session_satisfied_for_record(_record(cwd))
    assert out is None
    assert [e for e in _events(cwd) if e["type"] == "session_satisfied"] == []


def test_emit_noop_when_no_cwd(tmp_path):
    out = emit_session_satisfied_for_record(_record(None))
    assert out is None


def test_emit_reads_session_id_from_state_not_record(tmp_path):
    """The state file's session_id is the source of truth the stop hook compares
    against, so a stale record.session_id must not override it."""
    cwd = tmp_path / "repo"
    _write_state(cwd, session_id="state-sid")
    emit_session_satisfied_for_record(_record(cwd, session_id="record-sid"))
    ss = [e for e in _events(cwd) if e["type"] == "session_satisfied"]
    assert ss and ss[0]["data"]["session_id"] == "state-sid"


def test_emit_noop_when_state_pr_number_mismatches_record(tmp_path):
    """A recycled cwd: the state file in record.cwd now owns a DIFFERENT PR (a
    live session on another node). The emit must NOT nudge that session - it is
    not the owner of the merged PR we are reconciling."""
    cwd = tmp_path / "repo"
    _write_state(cwd, session_id="other-sid", pr_number=999)  # live session, different PR
    out = emit_session_satisfied_for_record(_record(cwd, pr_number=42))
    assert out is None
    assert [e for e in _events(cwd) if e["type"] == "session_satisfied"] == []


def test_emit_noop_when_cwd_is_non_string(tmp_path):
    """A corrupted/hand-edited graph could carry a non-string cwd; Path(non_str)
    would raise and abort the reconcile loop. The emit must treat it as no cwd."""
    rec = _record(tmp_path)
    rec.cwd = 12345  # type: ignore[assignment]
    # Must not raise; returns None (best-effort contract).
    assert emit_session_satisfied_for_record(rec) is None


def test_emit_is_non_fatal_when_append_event_raises(tmp_path, monkeypatch):
    """AC1-FR arm 1: a failing session_satisfied emit must be swallowed (return
    None, log to stderr) so the reconcile close is never aborted. The defensive
    stop-hook probe is the backstop that still completes the session."""
    cwd = tmp_path / "repo"
    _write_state(cwd, session_id="sid-1")

    import fno.events as events

    def _boom(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(events, "append_event", _boom)
    # Must not raise; returns None.
    assert emit_session_satisfied_for_record(_record(cwd)) is None


def test_emit_isolation_writes_only_to_owning_cwd(tmp_path):
    """Worktree binding: two sibling worktrees each with their own state file.
    Reconciling node A's record must write A's event ONLY to A's events.jsonl,
    bound to A's gate_state_hash - B's events.jsonl stays empty."""
    cwd_a = tmp_path / "wt-a"
    cwd_b = tmp_path / "wt-b"
    state_a = _write_state(cwd_a, session_id="sid-a", pr_number=42)
    _write_state(cwd_b, session_id="sid-b", pr_number=43)
    hash_a = _md5(state_a)

    out = emit_session_satisfied_for_record(_record(cwd_a, session_id="sid-a", pr_number=42))
    assert out is not None
    a_events = [e for e in _events(cwd_a) if e["type"] == "session_satisfied"]
    assert len(a_events) == 1
    assert a_events[0]["data"]["session_id"] == "sid-a"
    assert a_events[0]["data"]["gate_state_hash"] == hash_a
    # B is untouched.
    assert _events(cwd_b) == []


# ---------------------------------------------------------------------------
# CLI end-to-end: closing a drifted node emits the event (AC1-HP)
# ---------------------------------------------------------------------------

@pytest.fixture(autouse=True)
def _no_revert_fetch(monkeypatch):
    """Keep reconcile hermetic: never shell `gh pr list` from tests. W4 revert
    detection has its own unit tests (test_causal_fields.py)."""
    monkeypatch.setattr(rec, "fetch_recent_merged_prs", lambda **kw: [])


def _patch_graph_path(monkeypatch, graph_path: Path) -> None:
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", graph_path.parent / "graph.lock")
    monkeypatch.setattr(gc, "GRAPH_MD", graph_path.parent / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", graph_path.parent / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", graph_path.parent / "graph.lock")


def test_cli_reconcile_emits_session_satisfied_for_owner(tmp_path, monkeypatch):
    """AC1-HP: out-of-band merge -> node closed -> session_satisfied lands for
    the owning session_id in that session's events.jsonl."""
    graph_path = tmp_path / "graph.json"
    _patch_graph_path(monkeypatch, graph_path)
    sentinel_dir = tmp_path / "retro-pending"
    import fno.paths as paths
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sentinel_dir)

    owner_cwd = tmp_path / "owner-repo"
    state = _write_state(owner_cwd, session_id="owner-sid", pr_number=100)
    expected_hash = _md5(state)

    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(json.dumps({"entries": [{
        "id": "ab-hp", "title": "t", "pr_number": 100,
        "pr_url": "https://github.com/test-owner/test-repo/pull/100",
        "additional_prs": [], "completed_at": None, "superseded_by": None,
        "plan_path": None, "cwd": str(owner_cwd), "session_id": "owner-sid",
    }]}, indent=2) + "\n")

    monkeypatch.setattr(rec, "query_pr_merge_state", lambda n, repo=None, cwd=None: PrMergeState(
        number=n, state="MERGED", url=f"https://github.com/o/r/pull/{n}", merged_at="2026-06-02T00:00:00Z"))

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output

    ss = [e for e in _events(owner_cwd) if e["type"] == "session_satisfied"]
    assert len(ss) == 1, f"expected one session_satisfied, got {_events(owner_cwd)}"
    assert ss[0]["data"]["session_id"] == "owner-sid"
    assert ss[0]["data"]["source"] == "pr_merge"
    assert ss[0]["data"]["gate_state_hash"] == expected_hash


def test_cli_reconcile_no_emit_when_query_fails(tmp_path, monkeypatch):
    """AC1-ERR: a gh query failure closes nothing and emits no event."""
    graph_path = tmp_path / "graph.json"
    _patch_graph_path(monkeypatch, graph_path)
    sentinel_dir = tmp_path / "retro-pending"
    import fno.paths as paths
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sentinel_dir)

    owner_cwd = tmp_path / "owner-repo"
    _write_state(owner_cwd, session_id="owner-sid")

    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(json.dumps({"entries": [{
        "id": "ab-fail", "title": "t", "pr_number": 800,
        "pr_url": "https://github.com/test-owner/test-repo/pull/800",
        "additional_prs": [], "completed_at": None, "superseded_by": None,
        "plan_path": None, "cwd": str(owner_cwd), "session_id": "owner-sid",
    }]}, indent=2) + "\n")

    from fno.graph._reconcile import ReconcileError

    def _boom(number, repo=None, cwd=None):
        raise ReconcileError("gh auth required")

    monkeypatch.setattr(rec, "query_pr_merge_state", _boom)

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 4, result.output
    assert [e for e in _events(owner_cwd) if e["type"] == "session_satisfied"] == []
