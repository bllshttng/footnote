from __future__ import annotations

import pytest

from fno.events import ValidationError, validate


def test_transition_rejected_event_validates() -> None:
    event = {
        "ts": "2026-08-23T06:00:00Z",
        "type": "transition_rejected",
        "source": "hook",
        "data": {
            "session_id": "20260823T060900Z-cx73523-e04109",
            "kind": "invalid_transition",
            "event": "terminal_decided",
            "error": "invalid transition open + terminal_decided",
        },
    }
    assert validate(event) is None


@pytest.mark.parametrize("field", ["kind", "event", "from"])
def test_transition_rejected_rejects_unknown_enum_values(field: str) -> None:
    event = {
        "ts": "2026-08-23T06:00:00Z",
        "type": "transition_rejected",
        "source": "hook",
        "data": {
            "session_id": "20260823T060900Z-cx73523-e04109",
            "kind": "invalid_transition",
            "event": "terminal_decided",
            "from": "working",
            "error": "invalid",
        },
    }
    event["data"][field] = "not-a-real-value"
    with pytest.raises(ValidationError, match=f"unknown transition_rejected data.{field}"):
        validate(event)
