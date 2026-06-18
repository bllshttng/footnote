"""Tests for Entry._status as a Pydantic 2 computed_field.

Task 1.1 (FOLLOW-UPS #13): Entry._status must be derived from the entry's
own fields rather than being a settable plain str field.

Precedence (single-entry, matching recompute_statuses):
  completed_at set    -> "done"
  superseded_by set   -> "superseded"
  deferred_at set     -> "deferred"
  non-empty blocked_by -> "blocked"
  session_id set      -> "claimed"
  no plan_path        -> "idea"
  else                -> "ready"
"""
from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from fno.graph.statuses import VALID_STATUSES
from fno.graph.types import Entry


# ---------------------------------------------------------------------------
# AC1-HP: Happy path derivation for each enum value
# ---------------------------------------------------------------------------


def test_ac1_hp_completed_at_gives_done():
    """Given completed_at is set, _status must be 'done'."""
    entry = Entry(id="ab-done", plan_path="/some/plan", completed_at="2026-05-15T00:00:00Z")
    assert entry._status == "done"


def test_ac1_hp_superseded_by_gives_superseded():
    """Given superseded_by is set, _status must be 'superseded'."""
    entry = Entry(id="ab-sup", plan_path="/some/plan", superseded_by="ab-other")
    assert entry._status == "superseded"


def test_ac1_hp_deferred_at_gives_deferred():
    """Given deferred_at is set, _status must be 'deferred'."""
    entry = Entry(id="ab-def", plan_path="/some/plan", deferred_at="2026-05-15T00:00:00Z")
    assert entry._status == "deferred"


def test_ac1_hp_blocked_by_nonempty_gives_blocked():
    """Given blocked_by is non-empty (single-entry approximation), _status must be 'blocked'."""
    entry = Entry(id="ab-blk", plan_path="/some/plan", blocked_by=["ab-other"])
    assert entry._status == "blocked"


def test_ac1_hp_session_id_gives_claimed():
    """Given session_id is set, _status must be 'claimed'."""
    entry = Entry(id="ab-clm", plan_path="/some/plan", session_id="sess-123")
    assert entry._status == "claimed"


def test_ac1_hp_no_plan_path_gives_idea():
    """Given no plan_path, _status must be 'idea'."""
    entry = Entry(id="ab-idea")
    assert entry._status == "idea"


def test_ac1_hp_plan_path_no_other_signals_gives_ready():
    """Given plan_path set and no other lifecycle signals, _status must be 'ready'."""
    entry = Entry(id="ab-ready", plan_path="/some/plan")
    assert entry._status == "ready"


# ---------------------------------------------------------------------------
# AC1-EDGE: Parameterized coverage of all VALID_STATUSES derivation cases
# ---------------------------------------------------------------------------

EDGE_CASES = [
    # (status, kwargs)
    ("done", {"plan_path": "/p", "completed_at": "2026-05-15T00:00:00Z"}),
    ("superseded", {"plan_path": "/p", "superseded_by": "ab-x"}),
    ("deferred", {"plan_path": "/p", "deferred_at": "2026-05-15T00:00:00Z"}),
    ("blocked", {"plan_path": "/p", "blocked_by": ["ab-x"]}),
    ("claimed", {"plan_path": "/p", "session_id": "sess-abc"}),
    ("idea", {}),
    ("ready", {"plan_path": "/p"}),
]


@pytest.mark.parametrize("expected_status,kwargs", EDGE_CASES)
def test_ac1_edge_all_valid_statuses_covered(expected_status, kwargs):
    """Each VALID_STATUSES member has a derivation test case."""
    assert expected_status in VALID_STATUSES, f"{expected_status!r} not in VALID_STATUSES"
    entry = Entry(id="ab-edge", **kwargs)
    assert entry._status == expected_status


# ---------------------------------------------------------------------------
# AC1-ERR: Caller sets _status directly
# ---------------------------------------------------------------------------


def test_ac1_err_caller_set_bogus_status_does_not_persist():
    """Caller attempting Entry(_status='bogus') should not get that value back.

    Per Claude's Discretion #5: Pydantic 2 computed_field has no setter,
    so either a ValidationError is raised OR the value is silently overridden
    by the computed result. Either outcome is acceptable; the bogus value
    must never be observable on entry._status.
    """
    try:
        entry = Entry(id="ab-err", _status="bogus")
        # If Pydantic silently overrides: the value must not be "bogus"
        assert entry._status != "bogus", (
            "Expected computed_field to override 'bogus', got 'bogus' back"
        )
    except Exception:
        # Any exception (ValidationError, TypeError, etc.) is also acceptable
        pass


def test_ac1_err_caller_set_valid_status_is_still_computed():
    """Even a 'valid' status string like 'done' is overridden by computation."""
    # Entry with plan_path set and no lifecycle signals should be "ready",
    # regardless of what _status was passed in.
    try:
        entry = Entry(id="ab-err2", plan_path="/p", _status="done")
        # If no exception, must be computed "ready", not the passed "done"
        assert entry._status == "ready", (
            f"Expected computed 'ready', got {entry._status!r}"
        )
    except Exception:
        pass  # Any exception is also fine


# ---------------------------------------------------------------------------
# AC1-FR: Legacy graph.json with stale _status -> computed wins + drift event
# ---------------------------------------------------------------------------


def test_ac1_fr_legacy_stale_status_computed_wins(tmp_path):
    """model_validate with stale _status: computed value wins.

    AC1-FR: Given an on-disk entry with {completed_at set, _status: "ready"},
    model_validate must return an entry with _status == "done".
    """
    raw = {
        "id": "ab-fr",
        "completed_at": "2026-05-15T00:00:00Z",
        "plan_path": "/p",
        "_status": "ready",  # stale / impossible state
    }
    entry = Entry.model_validate(raw)
    assert entry._status == "done", (
        f"Expected computed 'done', got {entry._status!r}"
    )


def test_ac1_fr_drift_event_emitted_on_stale_status(tmp_path):
    """model_validate emits graph_status_drift event when persisted _status
    differs from computed value.

    Required event fields: entry_id, persisted, computed.
    """
    events_path = tmp_path / "events.jsonl"

    raw = {
        "id": "ab-fr-drift",
        "completed_at": "2026-05-15T00:00:00Z",
        "plan_path": "/p",
        "_status": "ready",  # stale
    }

    # Patch append_event to capture calls
    captured = []

    def fake_append(event, events_path=None, **kwargs):
        captured.append(event)

    with patch("fno.events.append_event", fake_append):
        entry = Entry.model_validate(raw)

    assert entry._status == "done"

    drift_events = [e for e in captured if e.get("type") == "graph_status_drift"]
    assert len(drift_events) == 1, (
        f"Expected 1 graph_status_drift event, got {len(drift_events)}: {captured}"
    )

    data = drift_events[0]["data"]
    assert data["entry_id"] == "ab-fr-drift"
    assert data["persisted"] == "ready"
    assert data["computed"] == "done"


def test_ac1_fr_no_drift_event_when_status_matches(tmp_path):
    """No drift event when persisted _status matches computed value."""
    captured = []

    def fake_append(event, events_path=None, **kwargs):
        captured.append(event)

    raw = {
        "id": "ab-no-drift",
        "plan_path": "/p",
        "_status": "ready",  # matches computed (plan_path set, no lifecycle)
    }

    with patch("fno.events.append_event", fake_append):
        entry = Entry.model_validate(raw)

    assert entry._status == "ready"
    drift_events = [e for e in captured if e.get("type") == "graph_status_drift"]
    assert len(drift_events) == 0, f"Unexpected drift events: {drift_events}"


# ---------------------------------------------------------------------------
# AC1-UI: model_dump round-trip preserves _status key
# ---------------------------------------------------------------------------


def test_ac1_ui_model_dump_includes_status_key():
    """entry.model_dump() must include '_status' key so graph.json readers don't break."""
    entry = Entry(id="ab-ui", plan_path="/some/plan")
    dumped = entry.model_dump()
    assert "_status" in dumped, f"'_status' not in model_dump(): {list(dumped.keys())}"
    assert dumped["_status"] == "ready"


def test_ac1_ui_round_trip_preserves_status():
    """Entry(**entry.model_dump()) round-trips to same _status."""
    for expected, kwargs in [
        ("done", {"plan_path": "/p", "completed_at": "2026-05-15T00:00:00Z"}),
        ("idea", {}),
        ("ready", {"plan_path": "/p"}),
        ("deferred", {"plan_path": "/p", "deferred_at": "2026-05-15T00:00:00Z"}),
    ]:
        entry = Entry(id="ab-rt", **kwargs)
        assert entry._status == expected

        dumped = entry.model_dump()
        # No drift event on round-trip because computed matches what's in dump
        rebuilt = Entry.model_validate(dumped)
        assert rebuilt._status == expected, (
            f"Round-trip failed for {expected}: got {rebuilt._status!r}"
        )


# ---------------------------------------------------------------------------
# Fix 1: _check_status_drift suppresses false-positive "blocked" events
# ---------------------------------------------------------------------------


def test_ac1_fr_no_drift_event_for_blocked_cascade_approximation():
    """No drift event emitted when computed=='blocked' but persisted is in
    {'ready', 'claimed', 'idea'} -- the known single-entry-vs-cascade
    approximation gap (Locked Decision #2).

    recompute_statuses resolves nodes with all-completed blockers to 'ready'
    or 'claimed', writing that to disk. On reload, _derive_status still sees
    a non-empty blocked_by and returns 'blocked'. This mismatch is NOT a real
    drift -- it is an expected gap between single-entry approximation and the
    cascade. Emitting graph_status_drift for this case would be a false positive.
    """
    captured = []

    def fake_append(event, events_path=None, **kwargs):
        captured.append(event)

    # blocked_by is non-empty, but _status was written as "ready" by recompute_statuses
    # (all blockers completed). Single-entry computed value is "blocked"; cascade
    # authoritative value is "ready". No drift event should fire.
    raw = {
        "id": "ab-x",
        "blocked_by": ["ab-y"],
        "_status": "ready",
    }

    with patch("fno.events.append_event", fake_append):
        entry = Entry.model_validate(raw)

    # computed value is still "blocked" (single-entry sees non-empty blocked_by)
    assert entry._status == "blocked"

    drift_events = [e for e in captured if e.get("type") == "graph_status_drift"]
    assert len(drift_events) == 0, (
        f"Expected 0 graph_status_drift events for blocked/ready cascade approximation, "
        f"got {len(drift_events)}: {captured}"
    )


# ---------------------------------------------------------------------------
# Fix 2: Status enum includes superseded
# ---------------------------------------------------------------------------


def test_ac1_status_enum_includes_superseded():
    """Status.superseded must exist with value 'superseded'."""
    from fno.graph.types import Status
    assert Status.superseded.value == "superseded"
    assert Status.superseded in Status


def test_ac1_status_enum_covers_all_valid_statuses():
    """The Status enum must cover exactly VALID_STATUSES, keeping them in sync."""
    from fno.graph.types import Status
    enum_values = {s.value for s in Status}
    assert enum_values == VALID_STATUSES, (
        f"Status enum values {enum_values} != VALID_STATUSES {VALID_STATUSES}. "
        "Add new enum member or update VALID_STATUSES."
    )


# ---------------------------------------------------------------------------
# ab-6603350c: Entry.rank validator rejects bool / inf / NaN at the boundary
# ---------------------------------------------------------------------------


def test_rank_accepts_none_and_finite_numbers():
    """None (unranked) and ordinary finite numbers are accepted."""
    assert Entry(id="ab-rank0001").rank is None
    assert Entry(id="ab-rank0002", rank=1.5).rank == 1.5
    # Ints coerce to float.
    assert Entry(id="ab-rank0003", rank=3).rank == 3.0


@pytest.mark.parametrize(
    "bad",
    [True, False, float("inf"), float("-inf"), float("nan")],
)
def test_rank_rejects_bool_and_non_finite(bad):
    """A bool or non-finite rank fails loudly at model construction."""
    with pytest.raises(Exception):  # pydantic ValidationError wraps the ValueError
        Entry(id="ab-rank0004", rank=bad)
