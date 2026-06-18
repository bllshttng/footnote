"""Megatron mission lifecycle event emissions.

Covers:
  - phase_transition typed-builder validates a megatron-source emission
  - mission_started, wave_advanced, mission_complete builders accept
    well-formed input and emit canonical envelopes
  - mission_complete rejects an unknown status enum at construction time
  - update_status() emits an event into a per-mission events.jsonl when
    a known transition fires (mission_started on running, mission_complete
    on terminal)
  - emit failure does NOT break update_status (telemetry must not block
    critical writes)
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno import events as abilities_events
from fno.megatron.state import (
    MissionState,
    update_status,
    write_state,
)


# -- Builder smoke tests --

def test_mission_started_builder():
    ev = abilities_events.mission_started(mission_id="m1")
    abilities_events.validate(ev)
    assert ev["type"] == "mission_started"
    assert ev["source"] == "megatron"
    assert ev["data"]["mission_id"] == "m1"


def test_wave_advanced_builder():
    ev = abilities_events.wave_advanced(
        mission_id="m1", wave=2, child_session_ids=["s1", "s2"]
    )
    abilities_events.validate(ev)
    assert ev["data"]["wave"] == 2
    assert ev["data"]["child_session_ids"] == ["s1", "s2"]


def test_mission_complete_rejects_bad_status():
    with pytest.raises(abilities_events.ValidationError, match=r"status"):
        abilities_events.mission_complete(mission_id="m1", status="sideways")


def test_mission_complete_accepts_valid_statuses():
    for status in ["done", "failed", "cancelled"]:
        ev = abilities_events.mission_complete(mission_id="m1", status=status)
        abilities_events.validate(ev)
        assert ev["data"]["status"] == status


# -- update_status integration --

def _bootstrap_pending_state(tmp_path: Path) -> Path:
    state_path = tmp_path / "state.md"
    state = MissionState(mission_id="m-001", status="pending")
    write_state(state_path, state)
    return state_path


def test_update_status_to_running_emits_mission_started(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".fno").mkdir()
    state_path = _bootstrap_pending_state(tmp_path)

    update_status(state_path, "running")

    events_file = tmp_path / ".fno/events.jsonl"
    assert events_file.exists(), "update_status did not emit"
    rows = [json.loads(l) for l in events_file.read_text().splitlines() if l.strip()]
    assert len(rows) == 1
    assert rows[0]["type"] == "mission_started"
    assert rows[0]["source"] == "megatron"
    assert rows[0]["data"]["mission_id"] == "m-001"


def test_update_status_to_complete_emits_mission_complete(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".fno").mkdir()
    state_path = _bootstrap_pending_state(tmp_path)

    update_status(state_path, "running")
    update_status(state_path, "complete")

    events_file = tmp_path / ".fno/events.jsonl"
    rows = [json.loads(l) for l in events_file.read_text().splitlines() if l.strip()]
    types = [r["type"] for r in rows]
    assert "mission_started" in types
    assert "mission_complete" in types
    completion = next(r for r in rows if r["type"] == "mission_complete")
    assert completion["data"]["status"] == "done"


def test_update_status_to_cancelled_emits_mission_complete(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".fno").mkdir()
    state_path = _bootstrap_pending_state(tmp_path)

    update_status(state_path, "cancelled")

    events_file = tmp_path / ".fno/events.jsonl"
    rows = [json.loads(l) for l in events_file.read_text().splitlines() if l.strip()]
    completion = next(r for r in rows if r["type"] == "mission_complete")
    assert completion["data"]["status"] == "cancelled"


def test_update_status_to_failed_emits_mission_complete(tmp_path, monkeypatch):
    """Schema enum allows mission_complete.status: failed; emit must cover
    every terminal state declared by the state machine. Without an
    explicit case, a future caller transitioning to ``failed`` would
    silently produce no event, leaving postmortem readers confused
    about whether the mission ended at all.
    """
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".fno").mkdir()
    state_path = _bootstrap_pending_state(tmp_path)

    update_status(state_path, "running")
    update_status(state_path, "failed")

    events_file = tmp_path / ".fno/events.jsonl"
    rows = [json.loads(l) for l in events_file.read_text().splitlines() if l.strip()]
    completion = next(r for r in rows if r["type"] == "mission_complete")
    assert completion["data"]["status"] == "failed"


def test_update_status_emit_failure_does_not_break_write(tmp_path, monkeypatch):
    """If the emit subsystem misbehaves, the state write must still succeed.

    Telemetry is observability, not a critical-write dependency. We force
    a failure by patching the emit helper to raise; update_status must
    swallow it and complete the status transition cleanly.
    """
    monkeypatch.chdir(tmp_path)
    state_path = _bootstrap_pending_state(tmp_path)

    from fno.megatron import state as state_mod

    def _broken_emit(*args, **kwargs):
        raise RuntimeError("simulated emit failure")

    monkeypatch.setattr(state_mod, "_emit_status_event", _broken_emit)

    # Should NOT raise; status transition completes regardless.
    update_status(state_path, "running")

    state_text = state_path.read_text()
    assert "status: running" in state_text


def test_update_status_no_abilities_dir_skips_emit(tmp_path, monkeypatch):
    """When cwd has no .fno/ folder, emit silently no-ops.

    update_status callers from inside test fixtures or non-repo cwds
    must not error when there's nowhere to write events.
    """
    monkeypatch.chdir(tmp_path)
    state_path = _bootstrap_pending_state(tmp_path)

    update_status(state_path, "running")

    assert not (tmp_path / ".fno/events.jsonl").exists()
