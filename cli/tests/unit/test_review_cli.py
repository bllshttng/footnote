"""``fno do review classify`` - the record builder the producers share.

The builder is the only writer of the attestation's finding record, so its
refusals and its truncation behavior are gate behaviors: a payload the
builder cannot read must refuse (never degrade to an empty record), and a
truncation must set the flag the gate reads as blocking for the remainder.
"""
from __future__ import annotations

import json

import pytest

from fno.review.cli import (
    _FINDINGS_COUNT_CAP,
    _RECORD_BYTE_BUDGET,
    RecordBuildError,
    build_emit_record,
)


def _finding(n: int, **overrides) -> dict:
    base = {
        "category": "correctness",
        "file": f"f{n}.py",
        "line": n,
        "summary": f"summary {n}",
        "failure_scenario": f"scenario {n}",
    }
    base.update(overrides)
    return base


class TestBuildEmitRecord:
    def test_bare_array_yields_counts_and_primitives(self) -> None:
        record = build_emit_record([_finding(1), _finding(2, category="typo")])
        assert record["findings_blocking"] == 1
        assert record["findings_nonblocking"] == 1
        assert len(record["findings"]) == 2
        assert record["findings"][0]["finding_key"] == "f1.py:1:correctness"

    def test_object_payload_reads_findings_key(self) -> None:
        record = build_emit_record({"findings": [_finding(1)]})
        assert record["findings_blocking"] == 1

    def test_object_payload_passes_dispositions_and_round_through(self) -> None:
        record = build_emit_record(
            {
                "findings": [_finding(1)],
                "dispositions": [
                    {"finding_key": "f1.py:1:correctness", "disposition": "fixed", "reason": "commit abc"}
                ],
                "review_round": 2,
            }
        )
        assert record["dispositions"] == [
            {"finding_key": "f1.py:1:correctness", "disposition": "fixed", "reason": "commit abc"}
        ]
        assert record["review_round"] == 2

    def test_bare_array_carries_no_dispositions_or_round(self) -> None:
        record = build_emit_record([_finding(1)])
        assert "dispositions" not in record
        assert "review_round" not in record

    def test_malformed_disposition_refuses(self) -> None:
        with pytest.raises(RecordBuildError):
            build_emit_record(
                {"findings": [], "dispositions": [{"finding_key": "x", "disposition": "vibes", "reason": "r"}]}
            )

    def test_disposition_without_reason_refuses(self) -> None:
        with pytest.raises(RecordBuildError):
            build_emit_record(
                {"findings": [], "dispositions": [{"finding_key": "x", "disposition": "declined", "reason": ""}]}
            )

    def test_negative_round_refuses(self) -> None:
        with pytest.raises(RecordBuildError):
            build_emit_record({"findings": [], "review_round": -1})

    def test_object_without_findings_key_refuses(self) -> None:
        with pytest.raises(RecordBuildError):
            build_emit_record({"oops": True})

    def test_count_cap_truncates_and_flags(self) -> None:
        payload = [_finding(n) for n in range(_FINDINGS_COUNT_CAP + 5)]
        record = build_emit_record(payload)
        assert len(record["findings"]) == _FINDINGS_COUNT_CAP
        assert record["findings_truncated"] is True
        assert record["findings_blocking"] == _FINDINGS_COUNT_CAP + 5

    def test_byte_budget_truncates_and_flags(self) -> None:
        # finding_key length (file:line:category) is unbounded, so it can blow
        # the budget on its own regardless of the bounded summary beside it.
        deep = [_finding(n, file="x" * 1200) for n in range(60)]
        record = build_emit_record(deep)
        assert len(record["findings"]) < 60
        assert record["findings_truncated"] is True

    def test_worst_case_summaries_stay_inside_the_envelope(self) -> None:
        """The primitives now carry a bounded summary, so the byte arithmetic
        changed. The RULE did not: an overflow truncates from the end and sets
        the flag the gate reads as blocking for the remainder.

        Measured rather than assumed, because this is the one thing carrying
        the finding text could break. Asserts the FLAG on the truncating side,
        never the absence of overflow: a builder that silently dropped every
        finding would also produce a record under budget.
        """
        from fno.review.findings import SUMMARY_MAX

        payload = [
            _finding(n, summary="s" * SUMMARY_MAX)
            for n in range(_FINDINGS_COUNT_CAP)
        ]
        record = build_emit_record(payload)
        serialized = len(json.dumps(record, ensure_ascii=False))
        assert serialized <= _RECORD_BYTE_BUDGET, serialized
        # Every finding survives. Carrying the summary must not cost capacity:
        # the array is a gate input and its truncation is BLOCKING.
        assert len(record["findings"]) == _FINDINGS_COUNT_CAP
        assert "findings_truncated" not in record
        # Detail was shed instead. Assert the shortened text is really there,
        # not merely that nothing overflowed - an emptied array also fits.
        kept = record["findings"][0]["summary"]
        assert kept and kept.startswith("s"), kept
        assert len(kept) < SUMMARY_MAX, len(kept)

    def test_summaries_are_shed_before_any_finding_is_dropped(self) -> None:
        """The regression this ordering exists to prevent.

        Adding a bounded summary to every primitive roughly tripled a
        primitive's serialized size, and `_RECORD_BYTE_BUDGET` - never the
        binding constraint before - started dropping findings that used to
        fit. That is not a cosmetic loss: the gate reads `findings_truncated`
        as a blocking remainder keyed `(truncated remainder)`, which no
        disposition can clear, so a large review made its own PR unmergeable.
        """
        from fno.review.findings import SUMMARY_MAX

        payload = [
            _finding(n, summary="s" * SUMMARY_MAX) for n in range(150)
        ]
        record = build_emit_record(payload)
        assert len(record["findings"]) == 150
        assert "findings_truncated" not in record

    def test_findings_still_drop_once_every_summary_is_gone(self) -> None:
        """Shedding detail is a budget escape hatch, never a way to fit
        anything. An unbounded `finding_key` still overflows, and when it does
        the array truncates and sets the flag the gate reads as blocking."""
        deep = [_finding(n, file="x" * 1200) for n in range(60)]
        record = build_emit_record(deep)
        assert len(record["findings"]) < 60
        assert record["findings_truncated"] is True
