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
        # reviewing/shipping pruned (x-f34f): zero consumers, no graph state.
        # `idea` sits below design: a decompose scaffold is a plan doc at a
        # pre-design rung, which the old axis had no word for.
        assert STATUS_PROGRESSION == (
            "idea",
            "design",
            "ready",
            "in_progress",
            "in_review",
        )

    def test_progression_is_tuple(self):
        assert isinstance(STATUS_PROGRESSION, tuple)


class TestValidateTransition:
    # Forward transitions - all must pass
    def test_AC1_HP_design_to_ready(self):
        validate_transition("design", "ready")  # no exception

    def test_AC1_HP_ready_to_in_progress(self):
        validate_transition("ready", "in_progress")

    def test_AC1_HP_in_progress_to_in_review(self):
        validate_transition("in_progress", "in_review")

    def test_AC1_HP_multi_step_skip_is_allowed(self):
        # design -> in_progress skips "ready" - forward multi-step is allowed
        validate_transition("design", "in_progress")

    def test_AC1_HP_design_to_in_review_extreme_skip(self):
        validate_transition("design", "in_review")

    # Backward transitions - must all raise
    def test_AC2_ERR_ready_to_design_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("ready", "design")

    def test_AC2_ERR_in_review_to_ready_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("in_review", "ready")

    def test_AC2_ERR_in_review_to_in_progress_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("in_review", "in_progress")

    def test_AC2_ERR_in_review_to_design_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("in_review", "design")

    # Identity transitions - must raise
    def test_AC3_ERR_identity_ready_to_ready_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("ready", "ready")

    def test_AC3_ERR_identity_design_to_design_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("design", "design")

    def test_AC3_ERR_identity_in_review_to_in_review_rejected(self):
        with pytest.raises(StatusTransitionError):
            validate_transition("in_review", "in_review")

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
            validate_transition("in_review", "ready")
        assert "in_review" in str(exc_info.value)
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


class TestRetiredSpellings:
    """x-3ad5: `shipped`/`archived` are retired as status VALUES and accepted on
    read, so a vault doc stamped under the old vocabulary keeps working.
    """

    def test_AC2_FR_alias_resolves_to_the_surviving_spelling(self):
        from fno.plan._status import canonical_status

        assert canonical_status("shipped") == "in_review"
        assert canonical_status("archived") == "superseded"
        # Quoting and casing are normalized the same way as any other status.
        assert canonical_status('  "Shipped" ') == "in_review"

    def test_AC2_FR_canonical_values_pass_through_untouched(self):
        from fno.plan._status import canonical_status

        for s in (*STATUS_PROGRESSION, "done", "superseded"):
            assert canonical_status(s) == s

    def test_AC2_FR_a_doc_on_the_old_spelling_still_coerces(self):
        assert coerce_status_from_yaml("shipped") == "in_review"

    def test_AC2_FR_transition_off_the_old_spelling_reads_as_its_survivor(self):
        # `shipped` ranks where `in_review` ranks, so a backward move still raises.
        with pytest.raises(StatusTransitionError):
            validate_transition("shipped", "ready")
        # ...and an identity move through the alias is still an identity move.
        with pytest.raises(StatusTransitionError):
            validate_transition("shipped", "in_review")

    def test_stub_is_the_retired_spelling_of_idea(self):
        """Scaffolds already on disk keep parsing; no migration pass ships."""
        from fno.plan._status import canonical_status

        assert canonical_status("stub") == "idea"
        assert canonical_status(' "Stub"  ') == "idea"
        assert coerce_status_from_yaml("stub") == "idea"
