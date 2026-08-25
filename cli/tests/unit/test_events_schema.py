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
