"""Tests for the canonical review invocation telemetry contract."""
from __future__ import annotations

from fno.events import validate


def _sent_event() -> dict:
    return {
        "ts": "2026-08-26T00:00:00Z",
        "type": "review_invocation",
        "source": "hook",
        "data": {
            "invocation_id": "ri-test-1",
            "stage": "sent",
            "verb": "/review",
            "args_raw": "medium --comment",
            "level": "medium",
            "level_source": "explicit",
            "flags": ["--comment"],
            "transport": "mux_pane_send_raw",
            "initiator": "self",
            "target_session_id": "target-test-1",
            "submit_required": True,
            "submit_key": "\\r",
            "submit_confirmed": False,
            "receipt": "text delivered, submission unconfirmed",
            "pr": 42,
            "head_sha": "a" * 40,
            "branch": "feature/review",
        },
    }


def test_review_invocation_event_is_a_valid_canonical_record() -> None:
    event = _sent_event()

    validate(event)

    assert event["type"] == "review_invocation"
    assert event["data"]["receipt"] == "text delivered, submission unconfirmed"

