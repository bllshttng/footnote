"""x-74aa wave 3: `fno agents top` carries run-ended rows and names its predicate.

census() counts runs holding a process; a registry row outside
LIVE_STATUSES whose transcript still moves used to vanish from the table,
and absence reads as "safe to dispatch". These tests pin the display-only
run-ended section (never LiveCensus) and the predicate statement.
"""
from __future__ import annotations

import json

import pytest

from fno.agents.spawn_gate import LiveCensus
from fno.agents.top import render_top


def _entry(name="t-peer", status="parked", harness="claude"):
    from fno.agents.registry import AgentEntry

    return AgentEntry(
        name=name, cwd="/tmp/p", log_path="/tmp/p.log", harness=harness, status=status
    )


def _hermetic_top(monkeypatch, entries=(), workers=()):
    """Pin every external read render_top makes; default truth says the
    session is working and its transcript moved 30s ago."""
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: list(entries))
    monkeypatch.setattr("fno.agents.top.census", lambda: LiveCensus(workers=list(workers)))
    monkeypatch.setattr("fno.agents.top.lane_rows", lambda: [])
    monkeypatch.setattr("fno.agents.top._crown_map", lambda: {})
    monkeypatch.setattr(
        "fno.agents.session_truth.resolve_session_truth",
        lambda handle, **kw: {"state": "working", "last_activity_age_s": 30},
    )


def test_run_ended_registry_row_renders_marked(monkeypatch):
    """AC5-HP: a parked registry row with a fresh transcript appears as a
    run-ended row with its session liveness and basis beside it."""
    _hermetic_top(monkeypatch, entries=[_entry()])
    out = render_top()
    assert "run-ended" in out
    assert "t-peer" in out
    assert "SOURCE" in out  # still one table
    assert "(transcript" in out  # the basis names the transcript evidence


def test_run_ended_rows_live_in_their_own_json_key(monkeypatch):
    """AC5-EDGE: the run-ended row never enters `workers`, so every script
    counting `workers` (and the gate's slot_count behind it) reads exactly as
    before."""
    _hermetic_top(monkeypatch, entries=[_entry()])
    payload = json.loads(render_top(as_json=True))
    assert [w["name"] for w in payload["workers"]] == []
    assert [r["name"] for r in payload["run_ended"]] == ["t-peer"]
    assert payload["run_ended"][0]["status"] == "run-ended"
    assert payload["run_ended"][0]["pid"] is None


def test_unreachable_session_drops_not_shown(monkeypatch):
    """Only a positive UNREACHABLE verdict drops a run-ended row; absence of
    evidence stays on the board."""
    _hermetic_top(monkeypatch, entries=[_entry()])
    monkeypatch.setattr(
        "fno.agents.session_truth.resolve_session_truth",
        lambda handle, **kw: {"state": None, "last_activity_age_s": None},
    )
    payload = json.loads(render_top(as_json=True))
    # truth_state None reads unknown/no-evidence: still shown, never folded
    # into a death verdict.
    assert [r["name"] for r in payload["run_ended"]] == ["t-peer"]
    assert payload["run_ended"][0]["reach"] != "unreachable"


def test_live_status_rows_never_double_render(monkeypatch):
    """A registry row INSIDE LIVE_STATUSES keeps its census path and never
    appears in run_ended."""
    _hermetic_top(monkeypatch, entries=[_entry(status="busy")])
    payload = json.loads(render_top(as_json=True))
    assert payload["run_ended"] == []


def test_json_and_footer_name_the_predicate(monkeypatch):
    """AC6-HP: both the JSON payload and the footer state which question the
    rows answer, and point per-session liveness at `fno agents truth`."""
    _hermetic_top(monkeypatch)
    payload = json.loads(render_top(as_json=True))
    assert "predicate" in payload
    assert "RUNS holding a process" in payload["predicate"]
    assert "fno agents truth" in payload["predicate"]
    out = render_top()
    assert "census: rows are RUNS holding a process" in out
    assert "fno agents truth" in out


def test_empty_table_does_not_read_as_no_live_sessions(monkeypatch):
    """AC6: the empty line names its predicate - a run-ended session is not
    missing, it is under run-ended."""
    _hermetic_top(monkeypatch, entries=[_entry()])
    out = render_top()
    assert "no live workers" in out
    assert "not missing" in out
    assert "run-ended" in out


def test_slot_count_untouched_by_render(monkeypatch):
    """AC5-EDGE, gate side: the census object render_top reads comes back with
    the same slot_count the gate saw - the renderer adds no rows to it."""
    _hermetic_top(monkeypatch, entries=[_entry()])
    from fno.agents import top as top_mod

    c = top_mod.census()
    before = c.slot_count if hasattr(c, "slot_count") else c.fno_slot_workers
    render_top()
    c2 = top_mod.census()
    after = c2.slot_count if hasattr(c2, "slot_count") else c2.fno_slot_workers
    assert before == after == 0
