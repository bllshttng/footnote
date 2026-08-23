from __future__ import annotations

from fno.events import validate


def test_transition_rejected_event_validates() -> None:
    event = {
        "ts": "2026-08-23T06:00:00Z",
        "type": "transition_rejected",
        "source": "hook",
        "data": {
            "session_id": "20260823T060900Z-cx73523-e04109",
            "event": "terminal_decided",
            "error": "invalid transition open + terminal_decided",
        },
    }
    assert validate(event) is None
