"""Tests for fno.events.log - event log with atomic append + audit."""
from __future__ import annotations

import json
import multiprocessing
import secrets
import time
from pathlib import Path
from typing import Any, Dict, List
from unittest.mock import patch

import pytest


# -- Helpers --

def _make_state_file(tmp_path: Path, session_id: str = "ses-abc123") -> Path:
    """Write a minimal target-state.md with a known session_id."""
    state_file = tmp_path / ".fno" / "target-state.md"
    state_file.parent.mkdir(parents=True, exist_ok=True)
    state_file.write_text(
        f"---\nsession_id: {session_id}\ncampaign_id: camp-001\nstatus: IN_PROGRESS\n---\n# body\n"
    )
    return state_file


def _events_file(tmp_path: Path) -> Path:
    return tmp_path / ".fno" / "events.jsonl"


# -- AC1-HP: emit writes one line per call --

def test_ac1_hp_emit_writes_one_line(tmp_path: Path) -> None:
    """AC1-HP: emit appends exactly one valid JSON line per call."""
    from fno.events.log import emit_event

    _make_state_file(tmp_path, "ses-abc123")
    events_file = _events_file(tmp_path)
    events_file.parent.mkdir(parents=True, exist_ok=True)

    nonce = emit_event(
        event_type="phase_transition",
        payload={"phase": "ship"},
        state_path=tmp_path / ".fno" / "target-state.md",
        events_path=events_file,
    )

    assert events_file.exists()
    lines = events_file.read_text().splitlines()
    assert len(lines) == 1
    event = json.loads(lines[0])

    # Required fields
    assert event["type"] == "phase_transition"
    assert event["session_id"] == "ses-abc123"
    assert event["campaign_id"] == "camp-001"
    assert "nonce" in event
    assert len(event["nonce"]) == 32  # secrets.token_hex(16) = 32 hex chars
    assert "ts" in event
    assert event["payload"] == {"phase": "ship"}

    # emit_event returns the nonce
    assert nonce == event["nonce"]


def test_ac1_hp_emit_appends_not_overwrites(tmp_path: Path) -> None:
    """AC1-HP: multiple emits produce multiple lines."""
    from fno.events.log import emit_event

    _make_state_file(tmp_path, "ses-abc123")
    events_file = _events_file(tmp_path)
    events_file.parent.mkdir(parents=True, exist_ok=True)

    emit_event("phase_init", {"phase": "build"},
               state_path=tmp_path / ".fno" / "target-state.md",
               events_path=events_file)
    emit_event("phase_transition", {"phase": "ship"},
               state_path=tmp_path / ".fno" / "target-state.md",
               events_path=events_file)

    lines = events_file.read_text().splitlines()
    assert len(lines) == 2
    e0 = json.loads(lines[0])
    e1 = json.loads(lines[1])
    assert e0["type"] == "phase_init"
    assert e1["type"] == "phase_transition"


# -- AC2-HP: emit is concurrency-safe --

def _worker_emit(args: tuple) -> None:
    """Worker function for concurrency test - importable at module level."""
    events_path_str, state_path_str, idx = args
    from fno.events.log import emit_event
    emit_event(
        event_type="phase_init",
        payload={"worker": idx},
        state_path=state_path_str,
        events_path=events_path_str,
    )


def test_ac2_hp_emit_concurrency_safe(tmp_path: Path) -> None:
    """AC2-HP: concurrent emits produce non-interleaved lines."""
    _make_state_file(tmp_path, "ses-concurrent")
    events_file = _events_file(tmp_path)
    events_file.parent.mkdir(parents=True, exist_ok=True)

    n_workers = 8
    args_list = [
        (str(events_file), str(tmp_path / ".fno" / "target-state.md"), i)
        for i in range(n_workers)
    ]

    with multiprocessing.Pool(n_workers) as pool:
        pool.map(_worker_emit, args_list)

    lines = events_file.read_text().splitlines()
    assert len(lines) == n_workers, f"Expected {n_workers} lines, got {len(lines)}"

    for line in lines:
        # Each line must be valid JSON (no interleaved bytes)
        event = json.loads(line)
        assert event["type"] == "phase_init"
        assert event["session_id"] == "ses-concurrent"


# -- AC3-HP: audit returns events for a session --

def test_ac3_hp_audit_returns_session_events(tmp_path: Path) -> None:
    """AC3-HP: audit filters by session_id and returns in order."""
    from fno.events.log import emit_event, read_events

    state_a = tmp_path / ".fno" / "state-a.md"
    state_a.parent.mkdir(parents=True, exist_ok=True)
    state_a.write_text("---\nsession_id: ses-A\ncampaign_id: camp-x\nstatus: IN_PROGRESS\n---\n")

    state_b = tmp_path / ".fno" / "state-b.md"
    state_b.write_text("---\nsession_id: ses-B\ncampaign_id: camp-x\nstatus: IN_PROGRESS\n---\n")

    events_file = _events_file(tmp_path)

    emit_event("phase_init", {"phase": "build"}, state_path=state_a, events_path=events_file)
    emit_event("phase_init", {"phase": "deploy"}, state_path=state_b, events_path=events_file)
    emit_event("gate_written", {"phase": "build"}, state_path=state_a, events_path=events_file)

    events_a = read_events(events_file, session_id="ses-A")
    assert len(events_a) == 2
    assert events_a[0]["type"] == "phase_init"
    assert events_a[1]["type"] == "gate_written"

    events_b = read_events(events_file, session_id="ses-B")
    assert len(events_b) == 1
    assert events_b[0]["type"] == "phase_init"


# -- AC4-HP: audit --strict detects gaps --

def test_ac4_hp_audit_strict_detects_gaps(tmp_path: Path) -> None:
    """AC4-HP: audit --strict returns ok:false when gate_written is missing for a phase."""
    from fno.events.log import emit_event, audit_session

    state_file = _make_state_file(tmp_path, "ses-gap")
    events_file = _events_file(tmp_path)

    # Emit phase_init for ship phase but NOT gate_written
    emit_event("phase_init", {"phase": "ship"}, state_path=state_file, events_path=events_file)

    result = audit_session(events_file, session_id="ses-gap", strict=True)
    assert result["ok"] is False
    assert "gaps" in result
    assert any("ship" in g and "gate_written" in g for g in result["gaps"])


def test_ac4_hp_audit_non_strict_no_gaps(tmp_path: Path) -> None:
    """AC4-HP: non-strict audit returns ok:true even with gaps."""
    from fno.events.log import emit_event, audit_session

    state_file = _make_state_file(tmp_path, "ses-nonstrict")
    events_file = _events_file(tmp_path)

    emit_event("phase_init", {"phase": "ship"}, state_path=state_file, events_path=events_file)

    result = audit_session(events_file, session_id="ses-nonstrict", strict=False)
    assert result["ok"] is True


def test_ac4_hp_audit_strict_passes_when_complete(tmp_path: Path) -> None:
    """AC4-HP: strict audit is ok when phase_init + gate_written both present."""
    from fno.events.log import emit_event, audit_session

    state_file = _make_state_file(tmp_path, "ses-complete")
    events_file = _events_file(tmp_path)

    emit_event("phase_init", {"phase": "ship"}, state_path=state_file, events_path=events_file)
    emit_event("gate_written", {"phase": "ship"}, state_path=state_file, events_path=events_file)

    result = audit_session(events_file, session_id="ses-complete", strict=True)
    assert result["ok"] is True


# -- Edge: events.jsonl auto-created --

def test_edge_events_file_auto_created(tmp_path: Path) -> None:
    """EDGE: events.jsonl is auto-created if missing."""
    from fno.events.log import emit_event

    state_file = _make_state_file(tmp_path, "ses-new")
    events_file = _events_file(tmp_path)
    assert not events_file.exists()

    emit_event("phase_init", {}, state_path=state_file, events_path=events_file)
    assert events_file.exists()


def test_edge_nonce_is_32_hex_chars(tmp_path: Path) -> None:
    """EDGE: nonce is exactly 32 lowercase hex chars (secrets.token_hex(16))."""
    from fno.events.log import emit_event

    state_file = _make_state_file(tmp_path, "ses-nonce")
    events_file = _events_file(tmp_path)

    nonce = emit_event("phase_init", {}, state_path=state_file, events_path=events_file)
    assert len(nonce) == 32
    assert all(c in "0123456789abcdef" for c in nonce)


# -- Task 1b.1: LegacyEvent TypedDict round-trip --

def test_legacy_event_roundtrip(tmp_path: Path) -> None:
    """AC-FR: LegacyEvent TypedDict importable; emit_event + read_events round-trip keeps all 6 keys."""
    from fno.events.log import LegacyEvent, emit_event, read_events

    # Write a minimal state.md with a known session_id
    state_file = tmp_path / "state.md"
    state_file.write_text("---\nsession_id: test-session-001\n---\n")
    events_file = tmp_path / "events.jsonl"

    emit_event(
        "test_event",
        {"phase": "init", "count": 3, "nested": {"k": "v"}},
        state_path=state_file,
        events_path=events_file,
    )

    events = read_events(events_file, session_id="test-session-001")

    assert len(events) == 1
    event = events[0]

    # All six keys present
    assert "type" in event
    assert "campaign_id" in event
    assert "session_id" in event
    assert "nonce" in event
    assert "ts" in event
    assert "payload" in event

    # Types match
    assert isinstance(event["type"], str)
    assert event["campaign_id"] is None or isinstance(event["campaign_id"], str)
    assert isinstance(event["session_id"], str)
    assert isinstance(event["nonce"], str)
    assert isinstance(event["ts"], str)
    assert isinstance(event["payload"], dict)

    # Values correct
    assert event["type"] == "test_event"
    assert event["session_id"] == "test-session-001"

    # Payload round-trips intact including nested dict
    assert event["payload"] == {"phase": "init", "count": 3, "nested": {"k": "v"}}
    assert event["payload"]["nested"] == {"k": "v"}
