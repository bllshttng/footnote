from __future__ import annotations

import pytest
from pydantic import ValidationError

from fno.company.contracts import EvidenceResult
from fno.company.join import JoinBranch, evaluate_join


def _branch(
    result: EvidenceResult, *, work_order_id: str = "wo-1", attempt_id: str = "att-1"
) -> JoinBranch:
    return JoinBranch(work_order_id=work_order_id, attempt_id=attempt_id, result=result)


def test_ac6_one_failed_branch_aggregates_failed_and_lists_all_rows() -> None:
    evaluation = evaluate_join(
        [
            _branch(EvidenceResult.PASSED, work_order_id="wo-1"),
            _branch(EvidenceResult.FAILED, work_order_id="wo-2"),
            _branch(EvidenceResult.PASSED, work_order_id="wo-3"),
        ]
    )
    assert evaluation.aggregate is EvidenceResult.FAILED
    assert len(evaluation.branches) == 3
    assert {row.work_order_id for row in evaluation.branches} == {"wo-1", "wo-2", "wo-3"}


def test_ac6_one_blocked_branch_aggregates_blocked() -> None:
    evaluation = evaluate_join(
        [
            _branch(EvidenceResult.PASSED, work_order_id="wo-1"),
            _branch(EvidenceResult.BLOCKED, work_order_id="wo-2"),
            _branch(EvidenceResult.PASSED, work_order_id="wo-3"),
        ]
    )
    assert evaluation.aggregate is EvidenceResult.BLOCKED


def test_ac6_one_branch_with_no_evidence_aggregates_unknown() -> None:
    evaluation = evaluate_join(
        [
            _branch(EvidenceResult.PASSED, work_order_id="wo-1"),
            _branch(EvidenceResult.UNKNOWN, work_order_id="wo-2"),
            _branch(EvidenceResult.PASSED, work_order_id="wo-3"),
        ]
    )
    assert evaluation.aggregate is EvidenceResult.UNKNOWN


def test_all_passed_aggregates_passed() -> None:
    evaluation = evaluate_join(
        [
            _branch(EvidenceResult.PASSED, work_order_id="wo-1"),
            _branch(EvidenceResult.PASSED, work_order_id="wo-2"),
        ]
    )
    assert evaluation.aggregate is EvidenceResult.PASSED


@pytest.mark.parametrize(
    "results",
    [
        [EvidenceResult.FAILED, EvidenceResult.PASSED, EvidenceResult.PASSED],
        [EvidenceResult.BLOCKED, EvidenceResult.PASSED],
        [EvidenceResult.UNKNOWN, EvidenceResult.PASSED],
    ],
)
def test_ac6_never_reports_passed_when_any_branch_is_non_passing(
    results: list[EvidenceResult],
) -> None:
    evaluation = evaluate_join(
        [_branch(r, work_order_id=f"wo-{i}") for i, r in enumerate(results)]
    )
    assert evaluation.aggregate is not EvidenceResult.PASSED


def test_failed_outranks_blocked_and_unknown() -> None:
    evaluation = evaluate_join(
        [
            _branch(EvidenceResult.BLOCKED, work_order_id="wo-1"),
            _branch(EvidenceResult.FAILED, work_order_id="wo-2"),
            _branch(EvidenceResult.UNKNOWN, work_order_id="wo-3"),
        ]
    )
    assert evaluation.aggregate is EvidenceResult.FAILED


def test_blocked_outranks_unknown() -> None:
    evaluation = evaluate_join(
        [
            _branch(EvidenceResult.BLOCKED, work_order_id="wo-1"),
            _branch(EvidenceResult.UNKNOWN, work_order_id="wo-2"),
        ]
    )
    assert evaluation.aggregate is EvidenceResult.BLOCKED


def test_empty_join_is_unknown_matching_delivery() -> None:
    # Mirrors delivery's "not results -> unknown": never claim passed on absent
    # evidence.
    evaluation = evaluate_join([])
    assert evaluation.aggregate is EvidenceResult.UNKNOWN
    assert evaluation.branches == ()


def test_rows_are_never_dropped() -> None:
    evaluation = evaluate_join(
        [
            _branch(EvidenceResult.PASSED, work_order_id="wo-1"),
            _branch(EvidenceResult.FAILED, work_order_id="wo-2"),
            _branch(EvidenceResult.UNKNOWN, work_order_id="wo-3"),
            _branch(EvidenceResult.BLOCKED, work_order_id="wo-4"),
        ]
    )
    assert len(evaluation.branches) == 4


def test_join_evaluation_is_frozen() -> None:
    evaluation = evaluate_join([_branch(EvidenceResult.PASSED)])
    with pytest.raises((ValidationError, TypeError)):
        evaluation.aggregate = EvidenceResult.FAILED  # type: ignore[misc]
