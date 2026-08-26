"""Tests for the canonical review invocation telemetry contract."""
from __future__ import annotations

from fno.events import validate
from fno.review.invocation import (
    adopt_pending_invocation,
    build_review_invocation_data,
    mint_invocation_id,
    pending_invocation_path,
    write_pending_invocation,
)


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


def test_review_invocation_data_omits_unknown_and_keeps_false_and_zero() -> None:
    data = build_review_invocation_data(
        invocation_id="ri-test-2",
        stage="started",
        verb="/review",
        submit_confirmed=False,
        subagent_count=0,
        model_family=None,
    )

    assert data["invocation_id"] == "ri-test-2"
    assert data["submit_confirmed"] is False
    assert data["subagent_count"] == 0
    assert "model_family" not in data


def test_pending_invocation_round_trips_a_positive_join_id(tmp_path) -> None:
    invocation_id = mint_invocation_id()

    assert invocation_id.startswith("ri-")
    assert write_pending_invocation(
        target_session_id="target-test-2",
        invocation_id=invocation_id,
        home=tmp_path,
    ) is True
    assert pending_invocation_path("target-test-2", home=tmp_path).is_file()
    assert adopt_pending_invocation("target-test-2", home=tmp_path) == invocation_id


def test_pending_invocation_write_is_fail_open(tmp_path) -> None:
    blocked_home = tmp_path / "blocked"
    blocked_home.write_text("positive-control", encoding="utf-8")
    invocation_id = mint_invocation_id()

    assert write_pending_invocation(
        target_session_id="target-test-3",
        invocation_id=invocation_id,
        home=blocked_home,
    ) is False
    assert invocation_id.startswith("ri-")
