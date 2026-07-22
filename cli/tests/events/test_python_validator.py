"""Tests for the schema-aware Python validator at fno.events.

Covers:
  - validate(event) success and failure paths
  - typed builders (phase_transition, child_promise, mission_*)
  - size cap enforcement (64KB on data payload)
  - keyword-only builder rejects unknown kwargs at construction time
  - SchemaUnavailableError if manifest absent (smoke-tested via monkeypatch)
"""
from __future__ import annotations

import pytest

from fno.events import (
    SchemaUnavailableError,
    ValidationError,
    child_promise,
    mission_complete,
    mission_started,
    phase_transition,
    validate,
    wave_advanced,
)


# -- AC1-HP: happy path --

def test_validate_happy_path() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "phase_transition",
        "source": "target",
        "data": {
            "gate_bearing": True,
            "gate": "ledger_updated",
            "phase": "register",
            "nonce": "abc",
            "session_id": "sess1",
        },
    }
    assert validate(event) is None


def test_validate_audit_only_phase_transition() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "phase_transition",
        "source": "fno-loop",
        "data": {
            "gate_bearing": False,
            "phase": "review",
            "nonce": "n",
            "session_id": "s",
        },
    }
    assert validate(event) is None


# -- AC2-ERR: required fields --

def test_validate_missing_source() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "phase_transition",
        "data": {
            "gate_bearing": True,
            "gate": "ledger_updated",
            "phase": "p",
            "nonce": "n",
            "session_id": "s",
        },
    }
    with pytest.raises(ValidationError, match="event missing required field: source"):
        validate(event)


def test_validate_missing_ts() -> None:
    event = {
        "type": "phase_transition",
        "source": "target",
        "data": {
            "gate_bearing": True,
            "gate": "ledger_updated",
            "phase": "p",
            "nonce": "n",
            "session_id": "s",
        },
    }
    with pytest.raises(ValidationError, match="event missing required field: ts"):
        validate(event)


def test_validate_unknown_source() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "phase_transition",
        "source": "bogus",
        "data": {
            "gate_bearing": True,
            "gate": "ledger_updated",
            "phase": "p",
            "nonce": "n",
            "session_id": "s",
        },
    }
    with pytest.raises(ValidationError, match=r"unknown source: 'bogus'"):
        validate(event)


def test_validate_unknown_type() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "made_up_type",
        "source": "target",
        "data": {},
    }
    with pytest.raises(ValidationError, match="unknown event type: made_up_type"):
        validate(event)


def test_validate_phase_transition_gate_bearing_without_gate() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "phase_transition",
        "source": "target",
        "data": {
            "gate_bearing": True,
            "phase": "p",
            "nonce": "n",
            "session_id": "s",
        },
    }
    with pytest.raises(ValidationError, match=r"gate_bearing=true must include data\.gate"):
        validate(event)


def test_validate_missing_data_field() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "child_promise",
        "source": "target",
        "data": {"session_id": "s"},
    }
    with pytest.raises(ValidationError, match=r"missing required data field: nonce"):
        validate(event)


# -- AC4-EDGE: size cap --

def test_validate_data_size_cap() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "phase_transition",
        "source": "target",
        "data": {
            "gate_bearing": True,
            "gate": "ledger_updated",
            "phase": "p",
            "nonce": "n",
            "session_id": "s",
            "blob": "x" * 70_000,
        },
    }
    with pytest.raises(ValidationError, match=r"data exceeds max_data_bytes"):
        validate(event)


# -- AC1-HP: typed builders --

def test_phase_transition_builder_happy() -> None:
    ev = phase_transition(
        gate="ledger_updated", phase="register", nonce="x", session_id="s", source="target"
    )
    assert ev["type"] == "phase_transition"
    assert ev["source"] == "target"
    assert ev["data"]["gate_bearing"] is True
    assert ev["data"]["gate"] == "ledger_updated"
    assert "ts" in ev


def test_phase_transition_builder_audit_only() -> None:
    ev = phase_transition(
        phase="review", nonce="x", session_id="s", source="fno-loop", gate_bearing=False
    )
    assert ev["data"]["gate_bearing"] is False
    assert "gate" not in ev["data"]


def test_phase_transition_rejects_unknown_kwarg() -> None:
    with pytest.raises(TypeError, match="unexpected keyword argument"):
        phase_transition(  # type: ignore[call-arg]
            gate="g", phase="p", nonce="n", session_id="s", source="target", whoops="extra"
        )


def test_child_promise_builder() -> None:
    ev = child_promise(session_id="s", nonce="n")
    assert ev["type"] == "child_promise"
    assert ev["source"] == "target"
    assert ev["data"] == {"session_id": "s", "nonce": "n"}


def test_mission_started_builder() -> None:
    ev = mission_started(mission_id="mt-001")
    assert ev["type"] == "mission_started"
    assert ev["source"] == "megatron"
    assert ev["data"]["mission_id"] == "mt-001"


def test_wave_advanced_builder() -> None:
    ev = wave_advanced(mission_id="mt-001", wave=2, child_session_ids=["s1", "s2"])
    assert ev["data"]["wave"] == 2
    assert ev["data"]["child_session_ids"] == ["s1", "s2"]


def test_mission_complete_builder() -> None:
    ev = mission_complete(mission_id="mt-001", status="done")
    assert ev["data"]["status"] == "done"


def test_mission_complete_rejects_bad_status() -> None:
    with pytest.raises(ValidationError, match=r"unknown status"):
        mission_complete(mission_id="mt-001", status="sideways")


def test_integrity_warning_builder_happy() -> None:
    from fno.events import integrity_warning

    ev = integrity_warning(
        kind="missing_nonce_legacy_accepted",
        phase="register",
        session_id="sess-77",
        artifact_path="/tmp/x.md",
    )
    assert ev["type"] == "integrity_warning"
    assert ev["source"] == "hook"
    assert ev["data"]["kind"] == "missing_nonce_legacy_accepted"
    assert ev["data"]["artifact_path"] == "/tmp/x.md"


def test_integrity_warning_rejects_unknown_kind() -> None:
    from fno.events import integrity_warning

    with pytest.raises(ValidationError, match=r"unknown integrity_warning kind"):
        integrity_warning(
            kind="bogus_kind_value",
            phase="register",
            session_id="sess-77",
            artifact_path="/tmp/x.md",
        )


def test_done_race_collision_builder_happy() -> None:
    from fno.events import done_race_collision

    ev = done_race_collision(
        node_id="ab-deadbeef",
        first_completed_at="2026-05-15T10:00:00+00:00",
        second_attempt_at="2026-05-15T12:00:00+00:00",
    )
    assert ev["type"] == "done_race_collision"
    assert ev["source"] == "fno-loop"
    assert ev["data"]["node_id"] == "ab-deadbeef"


# -- AC4-EDGE: SchemaUnavailableError on bad path --

def test_schema_unavailable_raises(monkeypatch, tmp_path) -> None:
    """Resolving with no sibling schema.yaml must raise SchemaUnavailableError.

    The loader reads ``schema.yaml`` beside the package module. We point the
    module's ``__file__`` at an empty tmp dir (no sibling schema) so the lookup
    misses; the live package schema other tests rely on is untouched.
    """
    import fno.events as events_mod

    fake_module = tmp_path / "events" / "__init__.py"
    fake_module.parent.mkdir(parents=True)
    monkeypatch.setattr(events_mod, "__file__", str(fake_module))
    with pytest.raises(SchemaUnavailableError, match="events schema not found"):
        events_mod._resolve_manifest_path()


# -- BUG-MT-001: megatron manifest events must validate --


def test_validate_accepts_manifest_baselined() -> None:
    """Regression: schema entry for manifest_baselined must exist so
    _emit_event in megatron/queue.py does not get swallowed by
    its outer except: pass via ValidationError."""
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_baselined",
        "source": "megatron",
        "data": {
            "mission_id": "ab-mission01",
            "sha_short": "abcdef012345",
        },
    }
    assert validate(event) is None


def test_validate_accepts_manifest_mutated() -> None:
    """Regression for BUG-MT-001 sibling event."""
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_mutated",
        "source": "megatron",
        "data": {
            "mission_id": "ab-mission01",
            "stored_sha_short": "111111111111",
            "fresh_sha_short": "222222222222",
        },
    }
    assert validate(event) is None


def test_validate_rejects_manifest_baselined_missing_mission_id() -> None:
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_baselined",
        "source": "megatron",
        "data": {"sha_short": "abcdef012345"},
    }
    with pytest.raises(ValidationError):
        validate(event)


def test_validate_rejects_manifest_mutated_missing_sha_fields() -> None:
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_mutated",
        "source": "megatron",
        "data": {"mission_id": "ab-mission01"},
    }
    with pytest.raises(ValidationError):
        validate(event)
