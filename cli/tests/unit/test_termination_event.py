from __future__ import annotations

import pytest

from fno.events import ValidationError, validate


TERMINATION_REASONS = (
    "DonePRGreen",
    "DoneUnreviewed",
    "DoneAdvisory",
    "DoneDelivery",
    "DoneBatched",
    "DoneAwaitingMerge",
    "DoneAwaitingReview",
    "DonePlanned",
    "NoWork",
    "Budget",
    "NoProgress",
    "Interrupted",
    "Aborted",
)


def _termination(reason: str) -> dict:
    return {
        "ts": "2026-08-23T06:00:00Z",
        "type": "termination",
        "source": "hook",
        "data": {"session_id": "session-1", "reason": reason, "message": "done"},
    }


@pytest.mark.parametrize("reason", TERMINATION_REASONS)
def test_all_termination_reasons_validate(reason: str) -> None:
    assert validate(_termination(reason)) is None


def test_unknown_termination_reason_fails_loud() -> None:
    with pytest.raises(ValidationError, match="unknown termination data.reason"):
        validate(_termination("DoneTypo"))
