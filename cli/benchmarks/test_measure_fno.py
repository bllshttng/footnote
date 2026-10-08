"""Tests for the fno-in-target measurement harness (Phase 01 gate).

One table per family; each row is a distinct branch of the decision rule or
the parser's strict-type contract.
"""

from __future__ import annotations

import subprocess
from unittest.mock import patch

import pytest

from measure_fno_in_target import apply_decision_rule, parse_session_data


def test_decision_rule_boundary_rows():
    """0.15 and 0.30 fall into the HIGHER bucket (>= boundaries)."""
    rows = [
        (0.10, "abort_daemon"),
        (0.149, "abort_daemon"),
        (0.15, "reads_only_v1"),
        (0.20, "reads_only_v1"),
        (0.299, "reads_only_v1"),
        (0.30, "full_v1"),
        (0.50, "full_v1"),
    ]
    for ratio, want in rows:
        assert apply_decision_rule(ratio) == want, ratio


_VALID_SESSION = {
    "session_id": "abc123",
    "fno_call_count": 5,
    "fno_wall_seconds": 1.0,
    "phase_wall_seconds": 20.0,
    "ratio": 0.05,
}


def test_parse_session_data_accepts_valid_and_rejects_bad_rows():
    result = parse_session_data(dict(_VALID_SESSION))
    assert result is not None
    assert result["session_id"] == "abc123"
    assert result["ratio"] == pytest.approx(0.05)

    # AC1-FR: malformed sessions return None (skipped), never raise.
    assert parse_session_data({"session_id": "partial"}) is None
    assert parse_session_data("not a dict") is None
    assert parse_session_data(None) is None
    assert parse_session_data(42) is None
    for seconds in (-1.0, 0.0):
        entry = {**_VALID_SESSION, "phase_wall_seconds": seconds}
        assert parse_session_data(entry) is None, seconds


def test_parse_session_data_strict_field_types():
    # ab-a111824 sigma-review HIGH: key presence was checked but not field
    # types; "banana" as a call count corrupted downstream arithmetic.
    for field, wrong_value in [
        ("session_id", 42),
        ("session_id", ""),
        ("fno_call_count", "banana"),
        ("fno_call_count", 1.5),
        ("fno_call_count", True),
        ("fno_wall_seconds", "1.0"),
        ("fno_wall_seconds", None),
        ("phase_wall_seconds", None),
        ("ratio", "0.05"),
        ("ratio", None),
    ]:
        entry = {**_VALID_SESSION, field: wrong_value}
        assert parse_session_data(entry) is None, (field, wrong_value)


def test_aggregate_ratio_rows():
    from measure_fno_in_target import compute_aggregate_ratio

    sessions = [
        {"fno_wall_seconds": 2.0, "phase_wall_seconds": 10.0},
        {"fno_wall_seconds": 1.0, "phase_wall_seconds": 20.0},
    ]
    assert compute_aggregate_ratio(sessions) == pytest.approx(0.10)
    assert compute_aggregate_ratio(
        [{"fno_wall_seconds": 6.0, "phase_wall_seconds": 20.0}]
    ) == pytest.approx(0.30)
    with pytest.raises(ValueError, match="no sessions"):
        compute_aggregate_ratio([])


class TestSubprocessHardening:
    """Sigma-review HIGH on PR for ab-f0fe4687: probe must drop failed runs."""

    def _make_completed(self, returncode: int, stderr: bytes = b""):
        return subprocess.CompletedProcess(
            args=["fno-py", "--help"], returncode=returncode, stdout=b"", stderr=stderr,
        )

    def test_probe_outcome_rows(self):
        from measure_fno_in_target import measure_median_fno_latency_ms

        with patch("measure_fno_in_target.subprocess.run") as mock_run:
            mock_run.return_value = self._make_completed(0)
            assert measure_median_fno_latency_ms(n_runs=20) >= 0.0

        # A timed-out probe is dropped, not counted as a fast 0ms sample.
        side_effects = [subprocess.TimeoutExpired(cmd="fno", timeout=10.0)] * 20
        with patch("measure_fno_in_target.subprocess.run", side_effect=side_effects):
            with pytest.raises(RuntimeError, match="timeout"):
                measure_median_fno_latency_ms(n_runs=20)

        for code, err in ((127, b"command not found"), (-9, b"")):
            side_effects = [self._make_completed(code, err)] * 20
            with patch("measure_fno_in_target.subprocess.run", side_effect=side_effects):
                with pytest.raises(RuntimeError):
                    measure_median_fno_latency_ms(n_runs=20)

        # Below the 25% failure threshold the median is still computed; above
        # it the run aborts.
        outcomes = ([self._make_completed(0)] * 16) + ([self._make_completed(1)] * 4)
        with patch("measure_fno_in_target.subprocess.run", side_effect=outcomes):
            assert measure_median_fno_latency_ms(n_runs=20) >= 0.0
        outcomes = ([self._make_completed(0)] * 10) + ([self._make_completed(1)] * 10)
        with patch("measure_fno_in_target.subprocess.run", side_effect=outcomes):
            with pytest.raises(RuntimeError, match=r"failed in 10/20"):
                measure_median_fno_latency_ms(n_runs=20)


def test_phase_scaled_call_count_rows():
    """Partial sessions are not charged for full-phase calls (PR #26 review)."""
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

    do_only = {
        "session_id": "do-only",
        "duration_minutes": 10.0,
        "phases_completed": ["do"],
    }
    result = build_session_measurement(do_only, median_fno_ms=200.0)
    assert result is not None
    measurement, _extras = result
    assert measurement["fno_call_count"] == max(
        1, round(TOTAL_CALLS_PER_FULL_SESSION / len(EXPECTED_PHASES_FULL))
    )

    # Phases outside EXPECTED_PHASES_FULL do not inflate the count.
    mixed = {
        "session_id": "mixed",
        "duration_minutes": 10.0,
        "phases_completed": ["think", "plan", "do", "review"],
    }
    result = build_session_measurement(mixed, median_fno_ms=200.0)
    assert result is not None
    measurement, _extras = result
    assert measurement["fno_call_count"] == max(
        1, round(TOTAL_CALLS_PER_FULL_SESSION * 2 / len(EXPECTED_PHASES_FULL))
    )

    ghost = {
        "session_id": "ghost",
        "duration_minutes": 10.0,
        "phases_completed": ["think", "plan"],
    }
    assert build_session_measurement(ghost, median_fno_ms=200.0) is None


def test_phase_0_decision_event_builder_rows():
    from fno.events import ValidationError, phase_0_decision, validate

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

    with pytest.raises(ValidationError):
        phase_0_decision(
            ratio=0.10,
            decision="not_a_real_bucket",
            evidence_path=".fno/measurements/x.md",
        )

    event = phase_0_decision(
        ratio=0.30, decision="full_v1", evidence_path="x.md", source="subagent"
    )
    validate(event)
    assert event["source"] == "subagent"
