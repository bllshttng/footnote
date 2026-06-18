"""Tests for section ownership allowlist (Task 1.2)."""

import pytest
from fno.plan._ownership import (
    BLUEPRINT_WRITE_ALLOWLIST,
    OwnershipViolation,
    assert_blueprint_can_write,
    check_blueprint_can_write,
)


class TestBlueprintWriteAllowlist:
    def test_AC1_HP_allowlist_is_frozenset(self):
        assert isinstance(BLUEPRINT_WRITE_ALLOWLIST, frozenset)

    def test_AC1_HP_allowlist_contains_execution_strategy(self):
        assert "Execution Strategy" in BLUEPRINT_WRITE_ALLOWLIST

    def test_AC1_HP_allowlist_contains_file_ownership_map(self):
        assert "File Ownership Map" in BLUEPRINT_WRITE_ALLOWLIST

    def test_AC1_HP_allowlist_contains_patterns_to_reuse(self):
        assert "Patterns to Reuse" in BLUEPRINT_WRITE_ALLOWLIST

    def test_AC1_HP_allowlist_contains_kill_criteria(self):
        assert "kill_criteria" in BLUEPRINT_WRITE_ALLOWLIST

    def test_AC1_HP_allowlist_contains_execution_mode(self):
        assert "execution_mode" in BLUEPRINT_WRITE_ALLOWLIST

    def test_AC1_HP_allowlist_contains_waves(self):
        assert "waves" in BLUEPRINT_WRITE_ALLOWLIST

    def test_AC1_HP_allowlist_has_exactly_six_entries(self):
        assert len(BLUEPRINT_WRITE_ALLOWLIST) == 6


class TestAssertBlueprintCanWrite:
    # Allowlist entries - must pass without raising
    def test_AC2_HP_execution_strategy_allowed(self):
        assert_blueprint_can_write("Execution Strategy")  # no exception

    def test_AC2_HP_file_ownership_map_allowed(self):
        assert_blueprint_can_write("File Ownership Map")

    def test_AC2_HP_patterns_to_reuse_allowed(self):
        assert_blueprint_can_write("Patterns to Reuse")

    def test_AC2_HP_kill_criteria_allowed(self):
        assert_blueprint_can_write("kill_criteria")

    def test_AC2_HP_execution_mode_allowed(self):
        assert_blueprint_can_write("execution_mode")

    def test_AC2_HP_waves_allowed(self):
        assert_blueprint_can_write("waves")

    # Non-allowlist entries - must raise OwnershipViolation
    def test_AC3_ERR_architecture_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Architecture")

    def test_AC3_ERR_overview_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Overview")

    def test_AC3_ERR_failure_modes_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Failure Modes")

    def test_AC3_ERR_user_stories_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("User Stories")

    def test_AC3_ERR_acceptance_criteria_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Acceptance Criteria")

    def test_AC3_ERR_locked_decisions_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Locked Decisions")

    def test_AC3_ERR_open_questions_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Open Questions")

    def test_AC3_ERR_domain_pitfalls_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Domain Pitfalls")

    # Case sensitivity
    def test_AC4_EDGE_lowercase_execution_strategy_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("execution strategy")

    def test_AC4_EDGE_uppercase_kill_criteria_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("Kill_criteria")

    # Empty string
    def test_AC4_EDGE_empty_string_rejected(self):
        with pytest.raises(OwnershipViolation):
            assert_blueprint_can_write("")

    # Error message quality
    def test_AC5_HP_error_message_includes_attempted_section(self):
        with pytest.raises(OwnershipViolation) as exc_info:
            assert_blueprint_can_write("Overview")
        assert "Overview" in str(exc_info.value)

    def test_AC5_HP_error_message_includes_allowlist(self):
        with pytest.raises(OwnershipViolation) as exc_info:
            assert_blueprint_can_write("Overview")
        msg = str(exc_info.value)
        # Allowlist entries (sorted) should appear in error
        assert "Execution Strategy" in msg
        assert "File Ownership Map" in msg
        assert "Patterns to Reuse" in msg
        assert "kill_criteria" in msg
        assert "execution_mode" in msg
        assert "waves" in msg


class TestCheckBlueprintCanWrite:
    def test_AC6_HP_returns_true_for_allowed(self):
        assert check_blueprint_can_write("Execution Strategy") is True
        assert check_blueprint_can_write("File Ownership Map") is True
        assert check_blueprint_can_write("Patterns to Reuse") is True
        assert check_blueprint_can_write("kill_criteria") is True
        assert check_blueprint_can_write("execution_mode") is True
        assert check_blueprint_can_write("waves") is True

    def test_AC6_HP_returns_false_for_disallowed(self):
        assert check_blueprint_can_write("Overview") is False
        assert check_blueprint_can_write("Architecture") is False
        assert check_blueprint_can_write("") is False

    def test_AC6_HP_does_not_raise(self):
        # non-raising variant must never raise
        result = check_blueprint_can_write("Overview")
        assert result is False


class TestOwnershipViolation:
    def test_is_ValueError_subclass(self):
        assert issubclass(OwnershipViolation, ValueError)
