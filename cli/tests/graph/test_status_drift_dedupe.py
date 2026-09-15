"""graph_status_drift emission: once per transition, and the open-do signal.

_check_status_drift appended a journal row on every deserialize of a
drifted node (2,241 rows for 23 ids in 35 minutes), and the single-entry
derivation disagreed with the store on every node holding an open do window.
"""
from __future__ import annotations

from unittest.mock import patch

import pytest

from fno.graph.types import Entry


@pytest.fixture(autouse=True)
def _reset_drift_dedupe():
    """Clear the process-wide emission dedupe around every test here.

    _DRIFT_EMITTED is module-global; without a reset a sibling test that
    validates the same (id, persisted, computed) first would suppress this
    test's expected emission, order-dependently.
    """
    from fno.graph import types as graph_types

    graph_types._DRIFT_EMITTED.clear()
    yield
    graph_types._DRIFT_EMITTED.clear()


def _drifted_raw(entry_id: str, persisted: str) -> dict:
    return {
        "id": entry_id,
        "completed_at": "2026-09-15T00:00:00Z",
        "plan_path": "/p",
        "status": persisted,  # stale: computed is "done"
    }


def test_drift_event_emitted_once_per_transition_per_process():
    """The same (entry_id, persisted, computed) validated twice in one process
    appends one journal row, not one per read."""
    captured = []

    def fake_append(event, events_path=None, **kwargs):
        captured.append(event)

    raw = _drifted_raw("ab-dedupe", "ready")

    with patch("fno.events.append_event", fake_append):
        Entry.model_validate(dict(raw))
        Entry.model_validate(dict(raw))

    drift_events = [e for e in captured if e.get("type") == "graph_status_drift"]
    assert len(drift_events) == 1, f"Expected 1 event, got {len(drift_events)}"


def test_drift_event_reemits_on_new_transition():
    """A changed persisted value is a new (entry_id, persisted, computed)
    transition and emits again."""
    captured = []

    def fake_append(event, events_path=None, **kwargs):
        captured.append(event)

    with patch("fno.events.append_event", fake_append):
        Entry.model_validate(_drifted_raw("ab-transition", "ready"))
        Entry.model_validate(_drifted_raw("ab-transition", "idea"))

    drift_events = [e for e in captured if e.get("type") == "graph_status_drift"]
    assert [(e["data"]["persisted"], e["data"]["computed"]) for e in drift_events] == [
        ("ready", "done"),
        ("idea", "done"),
    ]


def test_drift_emit_failure_retries_on_next_read():
    """A failed append must not mark the transition seen: the next read
    retries instead of losing the row for the process lifetime."""
    captured = []

    calls = []

    def flaky_append(event, events_path=None, **kwargs):
        calls.append(event)
        if len(calls) == 1:
            raise OSError("journal busy")
        captured.append(event)

    raw = _drifted_raw("ab-retry", "ready")

    with patch("fno.events.append_event", flaky_append):
        Entry.model_validate(dict(raw))
        Entry.model_validate(dict(raw))

    assert len(captured) == 1, f"Expected the retry to land one event, got {captured}"


def _do_session_raw(entry_id: str, ended_at: object) -> dict:
    row = {
        "phase": "do",
        "harness": "claude",
        "session_id": "s-1",
        "started_at": "2026-09-15T00:00:00Z",
    }
    if ended_at is not None:
        row["ended_at"] = ended_at
    return {
        "id": entry_id,
        "plan_path": "/p",
        "sessions": [row],
        "status": "in_progress",
    }


def test_open_do_session_row_holds_in_progress_without_drift():
    """Stored in_progress with no lock but one open do row is what the store
    derivation holds (graph_store.rs counts open do rows). The single-entry
    read must agree: no drift row, computed in_progress."""
    captured = []

    def fake_append(event, events_path=None, **kwargs):
        captured.append(event)

    with patch("fno.events.append_event", fake_append):
        entry = Entry.model_validate(_do_session_raw("ab-opendo", None))

    assert entry.status == "in_progress"
    drift_events = [e for e in captured if e.get("type") == "graph_status_drift"]
    assert len(drift_events) == 0, f"Expected 0 events, got {captured}"


def test_closed_do_row_reverts_to_plan_status_and_emits_once():
    """Once the do row carries ended_at the derivation reverts to the plan
    rung; a still-stale stored status then emits exactly one drift row."""
    captured = []

    def fake_append(event, events_path=None, **kwargs):
        captured.append(event)

    with patch("fno.events.append_event", fake_append):
        entry = Entry.model_validate(_do_session_raw("ab-closedo", "2026-09-15T01:00:00Z"))

    assert entry.status == "ready"
    drift_events = [e for e in captured if e.get("type") == "graph_status_drift"]
    assert len(drift_events) == 1, f"Expected 1 event, got {captured}"
    assert drift_events[0]["data"]["computed"] == "ready"
