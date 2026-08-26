"""Schema registration for the ``operator_decision`` event.

The event log is schema-validated: an unregistered type is refused at emit,
so the schema row is the prerequisite every producer depends on.
"""
from __future__ import annotations

import pytest

from fno.events import (
    EVENT_TYPES,
    ValidationError,
    decision_retracted,
    operator_decision,
    validate,
)


def _require_schema_loaded() -> None:
    if EVENT_TYPES is None:
        pytest.skip("event schema unavailable in this environment")


def test_operator_decision_registered_in_schema() -> None:
    _require_schema_loaded()
    assert "operator_decision" in EVENT_TYPES, "schema.yaml must register operator_decision"


def test_builder_passes_validate_with_required_fields() -> None:
    event = operator_decision(
        decision_id="d-ab12cd34",
        decision="fold every project's inbox",
    )
    validate(event)  # must not raise
    assert event["type"] == "operator_decision"
    assert event["data"]["decision_id"] == "d-ab12cd34"
    assert event["data"]["decision"] == "fold every project's inbox"


def test_builder_carries_the_full_record_shape() -> None:
    """The recovery fields survive the builder, not just the required pair."""
    event = operator_decision(
        decision_id="d-ab12cd34",
        decision="fold every project's inbox",
        subject="x-7d94",
        question_id="q-11112222",
        question="fold or migrate?",
        asked_by="cfc87e37",
        asked_at="2026-08-14T21:00:00Z",
        options=["fold", "migrate"],
        decided_by="cfc87e37",
        authority_source="operator",
        rationale="a fold is a read; you do not migrate before you can see",
        supersedes=None,
    )
    validate(event)
    data = event["data"]
    assert data["subject"] == "x-7d94"
    assert data["options"] == ["fold", "migrate"]
    assert data["authority_source"] == "operator"
    assert "supersedes" not in data  # None means omit


def test_validate_rejects_missing_required_field() -> None:
    _require_schema_loaded()
    event = {
        "ts": "2026-08-14T00:00:00Z",
        "type": "operator_decision",
        "source": "target",
        "data": {"decision_id": "d-ab12cd34"},
    }
    with pytest.raises(ValidationError, match="decision"):
        validate(event)


def test_decision_retracted_is_registered_and_carries_target() -> None:
    _require_schema_loaded()
    event = decision_retracted(
        target_decision_id="d-ab12cd34",
        subject="x-7d94",
        reason="the coordination window ended",
        retracted_by="worker-1",
        authority_source="agent",
    )
    validate(event)
    assert event["type"] == "decision_retracted"
    assert event["data"]["target_decision_id"] == "d-ab12cd34"
    assert event["data"]["reason"] == "the coordination window ended"


class TestReviewAttestationFindingRecord:
    """The optional finding record on review_attestation.

    Optional as a whole, but a PRESENT key must be the right shape: this is
    the record the coverage gate re-derives blocking from, and a silently
    malformed one is a forged count one level down.
    """

    @staticmethod
    def _event(**data):
        base = {
            "ts": "2026-08-25T23:00:00Z",
            "type": "review_attestation",
            "source": "hook",
            "data": {"reviewer": "code-review", "head_sha": "a" * 40, "verdict": "fail", "session_id": "s-1"},
        }
        base["data"].update(data)
        return base

    def test_record_carrying_event_validates(self) -> None:
        validate(
            self._event(
                findings_blocking=1,
                findings_nonblocking=1,
                findings=[
                    {
                        "category": "correctness",
                        "verdict": None,
                        "blocking": True,
                        "has_required_fields": True,
                        "finding_key": "a.py:3:correctness",
                    }
                ],
                review_round=2,
                dispositions=[
                    {"finding_key": "a.py:3:correctness", "disposition": "declined", "reason": "not applicable"}
                ],
            )
        )

    def test_negative_count_refused(self) -> None:
        with pytest.raises(ValidationError):
            validate(self._event(findings_blocking=-1))

    def test_findings_not_primitives_refused(self) -> None:
        with pytest.raises(ValidationError):
            validate(self._event(findings=[{"category": "correctness"}]))

    def test_disposition_outside_enum_refused(self) -> None:
        with pytest.raises(ValidationError):
            validate(
                self._event(
                    dispositions=[
                        {"finding_key": "a.py:3:correctness", "disposition": "handwaved", "reason": "trust me"}
                    ]
                )
            )

    def test_disposition_without_reason_refused(self) -> None:
        with pytest.raises(ValidationError):
            validate(
                self._event(
                    dispositions=[
                        {"finding_key": "a.py:3:correctness", "disposition": "declined", "reason": "  "}
                    ]
                )
            )

    def test_truncated_flag_must_be_boolean(self) -> None:
        with pytest.raises(ValidationError):
            validate(self._event(findings_truncated="yes"))

    def test_pre_existing_event_without_record_validates(self) -> None:
        validate(self._event())
