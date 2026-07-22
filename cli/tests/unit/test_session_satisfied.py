"""Tests for session_satisfied + auto_complete_triggered event types.

Covers:
  - typed builder happy paths
  - data.source enum enforcement at build time
  - required data fields enforced by validate()
  - optional evidence_url
  - keyword-only builder semantics
  - parity-corpus fixtures round-trip through validate()
"""
from __future__ import annotations

import pytest

from fno.events import (
    SESSION_SATISFIED_SOURCES,
    ValidationError,
    auto_complete_triggered,
    session_satisfied,
    validate,
)


def test_session_satisfied_sources_constant() -> None:
    assert SESSION_SATISFIED_SOURCES == frozenset(
        {"check_pr", "pr_merge", "ci_watcher", "abi_gate_manual", "delegated"}
    )


def test_session_satisfied_happy_path() -> None:
    event = session_satisfied(
        trigger="check_pr",
        reason="external_review_approved",
        session_id="s1",
        gate_state_hash="abc123",
        evidence_url="https://github.com/o/r/pull/1",
    )
    assert event["type"] == "session_satisfied"
    assert event["source"] == "target"
    assert event["data"]["source"] == "check_pr"
    assert event["data"]["reason"] == "external_review_approved"
    assert event["data"]["session_id"] == "s1"
    assert event["data"]["gate_state_hash"] == "abc123"
    assert event["data"]["evidence_url"] == "https://github.com/o/r/pull/1"


def test_session_satisfied_omits_evidence_url() -> None:
    event = session_satisfied(
        trigger="pr_merge",
        reason="pr_merged",
        session_id="s2",
        gate_state_hash="def456",
    )
    assert "evidence_url" not in event["data"]


def test_session_satisfied_alternate_envelope_source() -> None:
    event = session_satisfied(
        trigger="pr_merge",
        reason="pr_merged",
        session_id="s2",
        gate_state_hash="def456",
        source="fno-loop",
    )
    assert event["source"] == "fno-loop"


def test_session_satisfied_rejects_unknown_trigger() -> None:
    with pytest.raises(ValidationError, match="unknown session_satisfied trigger"):
        session_satisfied(
            trigger="bogus",
            reason="r",
            session_id="s",
            gate_state_hash="h",
        )


def test_session_satisfied_rejects_unknown_envelope_source() -> None:
    with pytest.raises(ValidationError, match="unknown source"):
        session_satisfied(
            trigger="check_pr",
            reason="r",
            session_id="s",
            gate_state_hash="h",
            source="bogus_envelope",
        )


def test_session_satisfied_keyword_only() -> None:
    with pytest.raises(TypeError):
        session_satisfied(  # type: ignore[misc]
            "check_pr", "r", "s", "h"
        )


def test_session_satisfied_unknown_kwarg_rejected() -> None:
    with pytest.raises(TypeError):
        session_satisfied(  # type: ignore[call-arg]
            trigger="check_pr",
            reason="r",
            session_id="s",
            gate_state_hash="h",
            extra="not-allowed",
        )


@pytest.mark.parametrize(
    "trigger", ["check_pr", "pr_merge", "ci_watcher", "abi_gate_manual", "delegated"]
)
def test_session_satisfied_all_triggers_validate(trigger: str) -> None:
    event = session_satisfied(
        trigger=trigger,
        reason="r",
        session_id="s",
        gate_state_hash="h",
    )
    assert validate(event) is None


def test_auto_complete_triggered_happy_path() -> None:
    event = auto_complete_triggered(trigger="check_pr", session_id="s1")
    assert event["type"] == "auto_complete_triggered"
    assert event["source"] == "hook"
    assert event["data"]["source"] == "check_pr"
    assert event["data"]["session_id"] == "s1"


def test_auto_complete_triggered_rejects_unknown_trigger() -> None:
    with pytest.raises(ValidationError, match="unknown auto_complete_triggered trigger"):
        auto_complete_triggered(trigger="bogus", session_id="s")


def test_auto_complete_triggered_keyword_only() -> None:
    with pytest.raises(TypeError):
        auto_complete_triggered("check_pr", "s")  # type: ignore[misc]


@pytest.mark.parametrize(
    "trigger", ["check_pr", "pr_merge", "ci_watcher", "abi_gate_manual", "delegated"]
)
def test_auto_complete_triggered_all_triggers_validate(trigger: str) -> None:
    event = auto_complete_triggered(trigger=trigger, session_id="s")
    assert validate(event) is None


def test_validate_session_satisfied_missing_gate_state_hash() -> None:
    event = {
        "ts": "2026-05-19T18:00:00Z",
        "type": "session_satisfied",
        "source": "target",
        "data": {
            "source": "check_pr",
            "reason": "r",
            "session_id": "s1",
        },
    }
    with pytest.raises(ValidationError, match="gate_state_hash"):
        validate(event)


def test_validate_auto_complete_triggered_missing_session_id() -> None:
    event = {
        "ts": "2026-05-19T18:00:00Z",
        "type": "auto_complete_triggered",
        "source": "hook",
        "data": {"source": "check_pr"},
    }
    with pytest.raises(ValidationError, match="session_id"):
        validate(event)


def test_validate_rejects_unknown_session_satisfied_data_source() -> None:
    """Shell emitters using `fno event emit --type session_satisfied --data ...`
    route through validate() not the typed builder. The enum must be enforced
    at the validator so a typo at the shell-emitter site fails fast instead
    of landing a noise event in events.jsonl."""
    event = {
        "ts": "2026-05-19T18:00:00Z",
        "type": "session_satisfied",
        "source": "target",
        "data": {
            "source": "typo_source",  # invalid - not in the enum
            "reason": "r",
            "session_id": "s",
            "gate_state_hash": "h",
        },
    }
    with pytest.raises(ValidationError, match="unknown session_satisfied data.source"):
        validate(event)


def test_validate_rejects_unknown_auto_complete_triggered_data_source() -> None:
    event = {
        "ts": "2026-05-19T18:00:00Z",
        "type": "auto_complete_triggered",
        "source": "hook",
        "data": {
            "source": "not_a_real_trigger",
            "session_id": "s",
        },
    }
    with pytest.raises(ValidationError, match="unknown auto_complete_triggered data.source"):
        validate(event)


def test_session_satisfied_rejects_empty_reason() -> None:
    """The audit-load-bearing reason field cannot be empty/whitespace."""
    with pytest.raises(ValidationError, match="reason cannot be empty"):
        session_satisfied(
            trigger="check_pr",
            reason="   ",
            session_id="s",
            gate_state_hash="h",
        )


def test_session_satisfied_rejects_empty_session_id() -> None:
    with pytest.raises(ValidationError, match="session_id cannot be empty"):
        session_satisfied(
            trigger="check_pr",
            reason="r",
            session_id="",
            gate_state_hash="h",
        )


def test_session_satisfied_rejects_empty_gate_state_hash() -> None:
    with pytest.raises(ValidationError, match="gate_state_hash cannot be empty"):
        session_satisfied(
            trigger="check_pr",
            reason="r",
            session_id="s",
            gate_state_hash="",
        )


def test_auto_complete_triggered_rejects_empty_session_id() -> None:
    with pytest.raises(ValidationError, match="session_id cannot be empty"):
        auto_complete_triggered(trigger="check_pr", session_id="")


# ── delegated source (Task 1.3) ───────────────────────────────────────────────


def test_session_satisfied_sources_includes_delegated() -> None:
    """AC1-HP: `delegated` must appear in SESSION_SATISFIED_SOURCES."""
    assert "delegated" in SESSION_SATISFIED_SOURCES


def test_session_satisfied_delegated_builder_succeeds() -> None:
    """AC1-HP: session_satisfied(trigger='delegated') builds without raising."""
    event = session_satisfied(
        trigger="delegated",
        reason="handoff_helper_archived_manifest",
        session_id="s-delegated",
        gate_state_hash="abc123",
    )
    assert event["type"] == "session_satisfied"
    assert event["data"]["source"] == "delegated"


def test_session_satisfied_delegated_validates() -> None:
    """AC1-HP: a delegated event round-trips through validate() cleanly."""
    event = session_satisfied(
        trigger="delegated",
        reason="handoff_helper_archived_manifest",
        session_id="s-delegated",
        gate_state_hash="abc123",
    )
    assert validate(event) is None


def test_auto_complete_triggered_delegated_succeeds() -> None:
    """AC1-HP: auto_complete_triggered(trigger='delegated') builds without raising."""
    event = auto_complete_triggered(trigger="delegated", session_id="s-delegated")
    assert event["data"]["source"] == "delegated"


def test_auto_complete_triggered_delegated_validates() -> None:
    """AC1-HP: a delegated auto_complete_triggered event validates cleanly."""
    event = auto_complete_triggered(trigger="delegated", session_id="s-delegated")
    assert validate(event) is None


def test_validate_delegated_data_source_accepted() -> None:
    """AC2-VERIFY: validate() at the schema level (shell-emitter path) accepts delegated."""
    event = {
        "ts": "2026-06-05T12:00:00Z",
        "type": "session_satisfied",
        "source": "target",
        "data": {
            "source": "delegated",
            "reason": "handoff_helper_archived_manifest",
            "session_id": "s-del",
            "gate_state_hash": "h",
        },
    }
    assert validate(event) is None
