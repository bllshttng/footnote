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


# ---- the disposition obligation ---------------------------------------------
#
# A findings-free pass attests nothing about EARLIER findings; emitting one
# over a branch that still holds non-terminal blocking findings is the
# silent producer half of the impossible-merge deadlock. Every chain here
# is constructed into the tmp repo the shared cap helper reads.


def _obligation_event(i: int, verdict: str, findings, dispositions=None) -> dict:
    data = {
        "reviewer": "code-review",
        "head_sha": f"{i:040x}",
        "verdict": verdict,
        "session_id": "s-ob",
        "branch": "feature/x-ob",
        "reviewed_base_sha": "a" * 40,
        "reviewed_head_sha": f"{i:040x}",
        "findings_blocking": len(findings),
        "findings": findings,
    }
    if dispositions:
        data["dispositions"] = dispositions
    return {"ts": f"2026-08-31T1{i:02d}:00:00Z", "type": "review_attestation",
            "source": "hook", "data": data}


_OB_HARD = {
    "category": "correctness",
    "verdict": "CONFIRMED",
    "blocking": True,
    "has_required_fields": True,
    "finding_key": "cli/src/fake.py:779:correctness",
}


def _seed_obligation_chain(tmp_path, events):
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in events) + "\n", encoding="utf-8"
    )


def test_a_findings_free_pass_over_an_undisposed_fail_refuses_by_key(tmp_path):
    _seed_obligation_chain(
        tmp_path, [_obligation_event(0, "fail", [_OB_HARD])]
    )
    with pytest.raises(RecordBuildError) as exc:
        build_emit_record(
            {"findings": []},
            verdict="pass",
            cwd=str(tmp_path),
            head_branch="feature/x-ob",
            head=f"{0:040x}",
        )
    assert "cli/src/fake.py:779:correctness" in str(exc.value)
    assert "dispositions" in str(exc.value)


def test_a_pass_carrying_the_fixed_disposition_is_produced_unchanged(tmp_path):
    _seed_obligation_chain(
        tmp_path, [_obligation_event(0, "fail", [_OB_HARD])]
    )
    record = build_emit_record(
        {
            "findings": [],
            "dispositions": [
                {
                    "finding_key": "cli/src/fake.py:779:correctness",
                    "disposition": "fixed",
                    "reason": "verified the fix delta",
                }
            ],
        },
        verdict="pass",
        cwd=str(tmp_path),
        head_branch="feature/x-ob",
        head=f"{0:040x}",
    )
    assert record["findings"] == []
    assert record["dispositions"][0]["disposition"] == "fixed"


def test_a_first_round_clean_pass_disposes_nothing_and_is_produced(tmp_path):
    _seed_obligation_chain(tmp_path, [])
    record = build_emit_record(
        {"findings": []},
        verdict="pass",
        cwd=str(tmp_path),
        head_branch="feature/x-ob",
        head=f"{0:040x}",
    )
    assert record["findings"] == []


def test_an_unreadable_event_log_produces_rather_than_refuses(monkeypatch, tmp_path):
    import fno.pr._coverage_gate as gate

    def _boom(*args, **kwargs):
        raise RuntimeError("instrument failure")

    monkeypatch.setattr(gate, "attestation_chain", _boom)
    record = build_emit_record(
        {"findings": []},
        verdict="pass",
        cwd=str(tmp_path),
        head_branch="feature/x-ob",
        head=f"{0:040x}",
    )
    assert record["findings"] == []


def test_a_fail_verdict_and_a_contextless_call_skip_the_obligation(tmp_path):
    _seed_obligation_chain(
        tmp_path, [_obligation_event(0, "fail", [_OB_HARD])]
    )
    # A fail record over the same chain: refused findings are its subject.
    record = build_emit_record(
        {"findings": []}, verdict="fail", cwd=str(tmp_path),
        head_branch="feature/x-ob", head=f"{0:040x}",
    )
    assert record["findings"] == []
    # No branch context: the chain cannot be scoped, so nothing is asked.
    contextless = build_emit_record({"findings": []}, verdict="pass")
    assert contextless["findings"] == []
