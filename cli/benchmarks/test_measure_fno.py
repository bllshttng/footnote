"""Tests for the fno-in-target measurement harness (Phase 01 gate).

TDD: these tests are written before measure_fno_in_target.py exists.
They exercise the decision-rule logic and error-handling paths.
"""
from __future__ import annotations

import subprocess
from unittest.mock import patch

import pytest


# ---------------------------------------------------------------------------
# Import target - will fail until the harness is written
# ---------------------------------------------------------------------------

from measure_fno_in_target import apply_decision_rule, parse_session_data


# ---------------------------------------------------------------------------
# AC1-EDGE: Boundary values at exactly 0.15 and 0.30
# ---------------------------------------------------------------------------


class TestDecisionRuleBoundaries:
    def test_ratio_below_abort_threshold(self):
        """ratio < 0.15 -> abort_daemon"""
        assert apply_decision_rule(0.10) == "abort_daemon"

    def test_ratio_at_abort_threshold_is_reads_only(self):
        """AC1-EDGE: ratio == 0.15 falls into reads_only_v1 (HIGHER bucket)"""
        assert apply_decision_rule(0.15) == "reads_only_v1"

    def test_ratio_between_thresholds(self):
        """0.15 <= ratio < 0.30 -> reads_only_v1"""
        assert apply_decision_rule(0.20) == "reads_only_v1"

    def test_ratio_at_full_v1_threshold(self):
        """AC1-EDGE: ratio == 0.30 falls into full_v1 (HIGHER bucket)"""
        assert apply_decision_rule(0.30) == "full_v1"

    def test_ratio_above_full_v1_threshold(self):
        """ratio > 0.30 -> full_v1"""
        assert apply_decision_rule(0.50) == "full_v1"

    def test_ratio_just_below_reads_only(self):
        """ratio = 0.149 -> abort_daemon (just under boundary)"""
        assert apply_decision_rule(0.149) == "abort_daemon"

    def test_ratio_just_below_full_v1(self):
        """ratio = 0.299 -> reads_only_v1 (just under boundary)"""
        assert apply_decision_rule(0.299) == "reads_only_v1"


# ---------------------------------------------------------------------------
# AC1-FR: Failure recovery - unparseable session skipped, continues
# ---------------------------------------------------------------------------


class TestParseSessionData:
    def test_valid_session_parsed(self):
        """A well-formed session dict is returned unchanged."""
        entry = {
            "session_id": "abc123",
            "fno_call_count": 5,
            "fno_wall_seconds": 1.0,
            "phase_wall_seconds": 20.0,
            "ratio": 0.05,
        }
        result = parse_session_data(entry)
        assert result is not None
        assert result["session_id"] == "abc123"
        assert result["ratio"] == pytest.approx(0.05)

    def test_missing_required_fields_returns_none(self):
        """AC1-FR: session missing required fields returns None (skipped)."""
        result = parse_session_data({"session_id": "partial"})
        assert result is None

    def test_non_dict_input_returns_none(self):
        """AC1-FR: non-dict input returns None (skipped)."""
        assert parse_session_data("not a dict") is None
        assert parse_session_data(None) is None
        assert parse_session_data(42) is None

    def test_negative_phase_wall_seconds_returns_none(self):
        """AC1-FR: nonsensical phase wall time returns None."""
        entry = {
            "session_id": "abc123",
            "fno_call_count": 5,
            "fno_wall_seconds": 1.0,
            "phase_wall_seconds": -1.0,
            "ratio": 0.05,
        }
        assert parse_session_data(entry) is None

    def test_zero_phase_wall_seconds_returns_none(self):
        """AC1-FR: zero phase wall time (would cause div-by-zero) returns None."""
        entry = {
            "session_id": "abc123",
            "fno_call_count": 0,
            "fno_wall_seconds": 0.0,
            "phase_wall_seconds": 0.0,
            "ratio": 0.0,
        }
        assert parse_session_data(entry) is None

    def test_wrong_field_types_returns_none(self):
        """ab-a1118224 sigma-review HIGH: type-design-analyzer found that key
        presence was checked but not field types. A ledger entry with
        ``"fno_call_count": "banana"`` or ``"ratio": None`` passed validation
        and corrupted downstream arithmetic. Pin the strict-type contract."""
        base = {
            "session_id": "abc123",
            "fno_call_count": 5,
            "fno_wall_seconds": 1.0,
            "phase_wall_seconds": 20.0,
            "ratio": 0.05,
        }
        # Each variant flips exactly one field to a wrong type.
        for field, wrong_value in [
            ("session_id", 42),               # int instead of str
            ("session_id", ""),               # empty str
            ("fno_call_count", "banana"),     # str instead of int
            ("fno_call_count", 1.5),          # float instead of int
            ("fno_call_count", True),         # bool (subclass of int)
            ("fno_wall_seconds", "1.0"),      # str instead of float
            ("fno_wall_seconds", None),       # None
            ("phase_wall_seconds", None),     # None
            ("ratio", "0.05"),                # str
            ("ratio", None),                  # None
        ]:
            entry = {**base, field: wrong_value}
            assert parse_session_data(entry) is None, (
                f"expected None for {field}={wrong_value!r}, got "
                f"{parse_session_data(entry)!r}"
            )

    def test_required_fields_derived_from_typeddict(self):
        """The _REQUIRED_SESSION_FIELDS tuple is derived from SessionData
        annotations so the two cannot drift. Pinning this avoids the
        feedback_dedup_change_check_all_entry_builders pattern."""
        from measure_fno_in_target import SessionData, _REQUIRED_SESSION_FIELDS

        assert set(_REQUIRED_SESSION_FIELDS) == set(SessionData.__annotations__)


# ---------------------------------------------------------------------------
# AC1-HP: Aggregate ratio computation
# ---------------------------------------------------------------------------


class TestAggregateRatio:
    """Verify ratio = sum(fno_wall_seconds) / sum(phase_wall_seconds)."""

    def test_aggregate_ratio_weighted(self):
        """Aggregate ratio is volume-weighted: sum(fno) / sum(phase)."""
        from measure_fno_in_target import compute_aggregate_ratio

        sessions = [
            {"fno_wall_seconds": 2.0, "phase_wall_seconds": 10.0},
            {"fno_wall_seconds": 1.0, "phase_wall_seconds": 20.0},
        ]
        # sum fno = 3.0, sum phase = 30.0, ratio = 0.10
        result = compute_aggregate_ratio(sessions)
        assert result == pytest.approx(0.10)

    def test_aggregate_ratio_single_session(self):
        """Single session: ratio = fno / phase."""
        from measure_fno_in_target import compute_aggregate_ratio

        sessions = [{"fno_wall_seconds": 6.0, "phase_wall_seconds": 20.0}]
        assert compute_aggregate_ratio(sessions) == pytest.approx(0.30)

    def test_aggregate_ratio_empty_raises(self):
        """Empty session list raises ValueError (no data to aggregate)."""
        from measure_fno_in_target import compute_aggregate_ratio

        with pytest.raises(ValueError, match="no sessions"):
            compute_aggregate_ratio([])


class TestSubprocessHardening:
    """Sigma-review HIGH on PR for ab-f0fe4687: probe must drop failed runs."""

    def _make_completed(self, returncode: int, stderr: bytes = b""):
        return subprocess.CompletedProcess(
            args=["fno", "--help"], returncode=returncode, stdout=b"", stderr=stderr,
        )

    def test_clean_run_returns_median(self):
        """20 successful probes -> median is finite and positive."""
        from measure_fno_in_target import measure_median_fno_latency_ms

        with patch("measure_fno_in_target.subprocess.run") as mock_run:
            mock_run.return_value = self._make_completed(0)
            result = measure_median_fno_latency_ms(n_runs=20)
        assert result >= 0.0

    def test_timeout_does_not_silently_succeed(self):
        """A timed-out probe is dropped, not counted as a fast 0ms sample."""
        from measure_fno_in_target import measure_median_fno_latency_ms

        side_effects = [subprocess.TimeoutExpired(cmd="fno", timeout=10.0)] * 20
        with patch("measure_fno_in_target.subprocess.run", side_effect=side_effects):
            with pytest.raises(RuntimeError, match="timeout"):
                measure_median_fno_latency_ms(n_runs=20)

    def test_nonzero_returncode_drops_sample(self):
        """A non-zero returncode probe is dropped, not averaged in."""
        from measure_fno_in_target import measure_median_fno_latency_ms

        side_effects = [
            self._make_completed(127, b"command not found"),
        ] * 20
        with patch("measure_fno_in_target.subprocess.run", side_effect=side_effects):
            with pytest.raises(RuntimeError, match="returncode|every attempt"):
                measure_median_fno_latency_ms(n_runs=20)

    def test_negative_returncode_signal_kill_is_failure(self):
        """SIGKILL (-9) returncode is treated as failure, not as a tiny latency."""
        from measure_fno_in_target import measure_median_fno_latency_ms

        side_effects = [self._make_completed(-9)] * 20
        with patch("measure_fno_in_target.subprocess.run", side_effect=side_effects):
            with pytest.raises(RuntimeError):
                measure_median_fno_latency_ms(n_runs=20)

    def test_partial_failure_below_threshold_continues(self):
        """If fewer than 25% of probes fail, median is still computed."""
        from measure_fno_in_target import measure_median_fno_latency_ms

        # 20 runs, 4 failures (20%) -> below 25% threshold; should succeed.
        outcomes = ([self._make_completed(0)] * 16) + ([self._make_completed(1)] * 4)
        with patch("measure_fno_in_target.subprocess.run", side_effect=outcomes):
            result = measure_median_fno_latency_ms(n_runs=20)
        assert result >= 0.0

    def test_partial_failure_above_threshold_raises(self):
        """If more than 25% of probes fail, RuntimeError aborts the run."""
        from measure_fno_in_target import measure_median_fno_latency_ms

        # 20 runs, 10 failures (50%) -> above 25% threshold; should raise.
        outcomes = ([self._make_completed(0)] * 10) + ([self._make_completed(1)] * 10)
        with patch("measure_fno_in_target.subprocess.run", side_effect=outcomes):
            with pytest.raises(RuntimeError, match=r"failed in 10/20"):
                measure_median_fno_latency_ms(n_runs=20)


class TestPhaseScaledCallCount:
    """Codex review on PR #268: partial sessions must not be charged for full-phase calls."""

    def test_full_session_uses_full_call_count(self):
        from measure_fno_in_target import (
            EXPECTED_PHASES_FULL,
            TOTAL_CALLS_PER_FULL_SESSION,
            build_session_measurement,
        )

        full = {
            "session_id": "full",
            "duration_minutes": 10.0,
            "phases_completed": list(EXPECTED_PHASES_FULL),
        }
        result = build_session_measurement(full, median_fno_ms=200.0)
        assert result is not None
        measurement, _extras = result
        assert measurement["fno_call_count"] == TOTAL_CALLS_PER_FULL_SESSION

    def test_do_only_session_scaled_to_one_sixth(self):
        from measure_fno_in_target import (
            EXPECTED_PHASES_FULL,
            TOTAL_CALLS_PER_FULL_SESSION,
            build_session_measurement,
        )

        do_only = {
            "session_id": "do-only",
            "duration_minutes": 10.0,
            "phases_completed": ["do"],
        }
        result = build_session_measurement(do_only, median_fno_ms=200.0)
        assert result is not None
        measurement, _extras = result
        expected = max(
            1,
            round(TOTAL_CALLS_PER_FULL_SESSION / len(EXPECTED_PHASES_FULL)),
        )
        assert measurement["fno_call_count"] == expected

    def test_unknown_phases_filtered_before_scaling(self):
        """Phases not in EXPECTED_PHASES_FULL (e.g. 'think', 'plan') don't inflate the count."""
        from measure_fno_in_target import (
            EXPECTED_PHASES_FULL,
            TOTAL_CALLS_PER_FULL_SESSION,
            build_session_measurement,
        )

        mixed = {
            "session_id": "mixed",
            "duration_minutes": 10.0,
            "phases_completed": ["think", "plan", "do", "review"],
        }
        result = build_session_measurement(mixed, median_fno_ms=200.0)
        assert result is not None
        measurement, _extras = result
        # Only do + review count -> 2/6 of full
        expected = max(
            1,
            round(TOTAL_CALLS_PER_FULL_SESSION * 2 / len(EXPECTED_PHASES_FULL)),
        )
        assert measurement["fno_call_count"] == expected

    def test_no_recognized_phases_returns_none(self):
        from measure_fno_in_target import build_session_measurement

        none_recognized = {
            "session_id": "ghost",
            "duration_minutes": 10.0,
            "phases_completed": ["think", "plan"],  # neither in EXPECTED_PHASES_FULL
        }
        assert build_session_measurement(none_recognized, median_fno_ms=200.0) is None


class TestPhase0DecisionEvent:
    """Verify the canonical phase_0_decision builder produces a schema-valid event."""

    def test_builder_produces_schema_valid_event(self):
        from fno.events import phase_0_decision, validate

        event = phase_0_decision(
            ratio=0.10,
            decision="abort_daemon",
            evidence_path=".fno/measurements/x.md",
        )
        validate(event)
        assert event["type"] == "phase_0_decision"
        assert event["source"] == "target"
        assert event["data"]["ratio"] == pytest.approx(0.10)
        assert event["data"]["decision"] == "abort_daemon"

    def test_builder_rejects_unknown_decision(self):
        from fno.events import ValidationError, phase_0_decision

        with pytest.raises(ValidationError):
            phase_0_decision(
                ratio=0.10,
                decision="not_a_real_bucket",
                evidence_path=".fno/measurements/x.md",
            )

    def test_builder_supports_subagent_source(self):
        from fno.events import phase_0_decision, validate

        event = phase_0_decision(
            ratio=0.30,
            decision="full_v1",
            evidence_path="x.md",
            source="subagent",
        )
        validate(event)
        assert event["source"] == "subagent"
