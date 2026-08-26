"""``fno do review classify`` - the record builder the producers share.

The builder is the only writer of the attestation's finding record, so its
refusals and its truncation behavior are gate behaviors: a payload the
builder cannot read must refuse (never degrade to an empty record), and a
truncation must set the flag the gate reads as blocking for the remainder.
"""
from __future__ import annotations

import pytest

from fno.review.cli import (
    _FINDINGS_COUNT_CAP,
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
        # The primitives carry no summary text, so the byte budget bites on
        # finding_key length (file:line:category), the one unbounded field.
        deep = [_finding(n, file="x" * 1200) for n in range(60)]
        record = build_emit_record(deep)
        assert len(record["findings"]) < 60
        assert record["findings_truncated"] is True
