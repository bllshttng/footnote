"""Tests for frontmatter status state machine (Task 1.2)."""

import pytest
from fno.plan._status import (
    STATUS_PROGRESSION,
    StatusTransitionError,
    coerce_status_from_yaml,
    validate_transition,
)


class TestStatusProgression:
    def test_progression_contains_all_expected_statuses(self):
        assert STATUS_PROGRESSION == (
            "design",
            "ready",
            "in_progress",
            "reviewing",
            "shipping",
            "shipped",
        )

    def test_progression_is_tuple(self):
        assert isinstance(STATUS_PROGRESSION, tuple)


class TestValidateTransition:
    # Forward transitions - all must pass
    def test_AC1_HP_design_to_ready(self):
        validate_transition("design", "ready")  # no exception

    def test_AC1_HP_ready_to_in_progress(self):
        validate_transition("ready", "in_progress")

    def test_AC1_HP_in_progress_to_reviewing(self):
        validate_transition("in_progress", "reviewing")

    def test_AC1_HP_reviewing_to_shipping(self):
        validate_transition("reviewing", "shipping")

    def test_AC1_HP_shipping_to_shipped(self):
        validate_transition("shipping", "shipped")

    def test_AC1_HP_multi_step_skip_is_allowed(self):
        # design -> in_progress skips "ready" - forward multi-step is allowed
        validate_transition("design", "in_progress")

    def test_AC1_HP_design_to_shipped_extreme_skip(self):
        validate_transition("design", "shipped")

    # Backward transitions - must all raise
    def test_AC2_ERR_ready_to_design_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("ready", "design")

    def test_AC2_ERR_shipped_to_ready_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("shipped", "ready")

    def test_AC2_ERR_reviewing_to_in_progress_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("reviewing", "in_progress")

    def test_AC2_ERR_shipped_to_design_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("shipped", "design")

    # Identity transitions - must raise
    def test_AC3_ERR_identity_ready_to_ready_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("ready", "ready")

    def test_AC3_ERR_identity_design_to_design_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("design", "design")

    def test_AC3_ERR_identity_shipped_to_shipped_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("shipped", "shipped")

    # Unknown statuses - must raise
    def test_AC4_ERR_unknown_old_status_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("draft", "ready")

    def test_AC4_ERR_unknown_new_status_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("ready", "wip")

    def test_AC4_ERR_both_unknown_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("draft", "wip")

    def test_AC4_EDGE_error_message_is_informative(self):
        with pytest.raises(StatusTransitionError) as exc_info:
            validate_transition("shipped", "ready")
        assert "shipped" in str(exc_info.value)
        assert "ready" in str(exc_info.value)


class TestCoerceStatusFromYaml:
    # YAML bool coercion (per feedback_literal_string_rejects_yaml_bool)
    def test_AC5_HP_true_bool_coerces_to_string_then_rejected(self):
        # True -> "true" which is not in STATUS_PROGRESSION -> StatusTransitionError
        with pytest.raises(StatusTransitionError):
            coerce_status_from_yaml(True)

    def test_AC5_HP_false_bool_coerces_to_string_then_rejected(self):
        # False -> "false" which is not in STATUS_PROGRESSION -> StatusTransitionError
        with pytest.raises(StatusTransitionError):
            coerce_status_from_yaml(False)

    def test_AC5_HP_none_raises(self):
        with pytest.raises((StatusTransitionError, TypeError, ValueError)):
            coerce_status_from_yaml(None)

    def test_AC5_HP_valid_status_string_passes(self):
        result = coerce_status_from_yaml("design")
        assert result == "design"

    def test_AC5_HP_all_valid_statuses_pass(self):
        for status in STATUS_PROGRESSION:
            result = coerce_status_from_yaml(status)
            assert result == status

    def test_AC5_ERR_invalid_string_rejected(self):
        with pytest.raises(StatusTransitionError):
            coerce_status_from_yaml("in-progress")  # hyphen variant

    def test_AC5_ERR_unknown_string_rejected(self):
        with pytest.raises(StatusTransitionError):
            coerce_status_from_yaml("wip")

    def test_StatusTransitionError_is_ValueError_subclass(self):
        assert issubclass(StatusTransitionError, ValueError)
