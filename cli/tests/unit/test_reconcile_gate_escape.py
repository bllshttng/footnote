"""Tier-1 gate_escape auto-emit on reconcile oob-merge close (x-f894).

Exercises the #222 boundary (the load-bearing correctness surface) and the
fail-open + emit-failure-visible contract, driving the pure helper with an
injected reviews_fetcher so no gh/config is needed.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.graph._reconcile import MergeDriftRecord, emit_gate_escape_for_record


def _record(cwd: Path, *, pr: int = 218, node: str = "x-cccc") -> MergeDriftRecord:
    return MergeDriftRecord(
        node_id=node,
        plan_path=None,
        pr_number=pr,
        pr_url="https://github.com/owner/repo/pull/%d" % pr,
        pr_state="MERGED",
        merged_at="2026-07-07T00:00:00Z",
        error=None,
        session_id="sess-1",
        cwd=str(cwd),
    )


def _events(cwd: Path) -> list[dict]:
    p = cwd / ".fno" / "events.jsonl"
    if not p.exists():
        return []
    return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]


def _gate_escapes(cwd: Path) -> list[dict]:
    return [e for e in _events(cwd) if e.get("type") == "gate_escape"]


def test_ac1_hp_emits_on_required_bot_never_reviewed(tmp_path):
    """AC1-HP: required bot never reviewed an oob-merged PR -> one dead-bot."""
    rec = _record(tmp_path)
    emit_gate_escape_for_record(
        rec, required_bots=["codex"], reviews_fetcher=lambda *a, **k: set()
    )
    escapes = _gate_escapes(tmp_path)
    assert len(escapes) == 1
    data = escapes[0]["data"]
    assert data["reason"] == "dead-bot"
    assert data["pr"] == 218
    assert data["graph_node_id"] == "x-cccc"
    assert "codex" in data["detail"]


def test_ac2_edge_no_emit_when_no_required_bots(tmp_path):
    """AC2-EDGE (#222): a no-required-bots repo self-merge is NOT an escape."""
    rec = _record(tmp_path)
    emit_gate_escape_for_record(
        rec, required_bots=[], reviews_fetcher=lambda *a, **k: set()
    )
    assert _gate_escapes(tmp_path) == []


def test_ac2b_edge_no_emit_when_required_bot_reviewed(tmp_path):
    """AC2b-EDGE: the required bot DID review; only the merge was oob -> no escape."""
    rec = _record(tmp_path)
    emit_gate_escape_for_record(
        rec, required_bots=["Codex"], reviews_fetcher=lambda *a, **k: {"codex"}
    )
    assert _gate_escapes(tmp_path) == []


def test_ac4_inv_no_double_count_same_pr_reason(tmp_path):
    """AC4-INV: two closes racing the same events.jsonl count the escape once."""
    rec = _record(tmp_path)
    for _ in range(2):
        emit_gate_escape_for_record(
            rec, required_bots=["codex"], reviews_fetcher=lambda *a, **k: set()
        )
    assert len(_gate_escapes(tmp_path)) == 1


def test_ac5_err_fail_open_and_ac7_failure_logged(tmp_path, monkeypatch):
    """AC5-ERR: a failed emit never raises. AC7-FR: it is logged durably."""
    import fno.events as events_mod

    def _boom(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(events_mod, "append_event", _boom)
    rec = _record(tmp_path)
    # Must NOT raise (fail open).
    out = emit_gate_escape_for_record(
        rec, required_bots=["codex"], reviews_fetcher=lambda *a, **k: set()
    )
    assert out is None
    assert _gate_escapes(tmp_path) == []  # nothing landed
    # ...but the failure is visible in the durable counter (AC7).
    fail_log = tmp_path / ".fno" / "gate_escape_emit_failures.jsonl"
    assert fail_log.exists()
    lines = [json.loads(x) for x in fail_log.read_text().splitlines() if x.strip()]
    assert len(lines) == 1
    assert lines[0]["reason"] == "dead-bot"


def test_review_fetch_failure_is_a_logged_blind_spot(tmp_path):
    """A review-fetch failure means we can't tell -> fail open (no emit) but log
    the blind spot so retro surfaces it, not a silent low reading (AC7)."""
    def _raise(*a, **k):
        raise RuntimeError("gh auth expired")

    rec = _record(tmp_path)
    out = emit_gate_escape_for_record(
        rec, required_bots=["codex"], reviews_fetcher=_raise
    )
    assert out is None
    assert _gate_escapes(tmp_path) == []
    assert (tmp_path / ".fno" / "gate_escape_emit_failures.jsonl").exists()
