"""Join evaluation over required campaign branches.

A join aggregates the evidence results of a campaign's required branches using
the same precedence delivery uses for requirement rows (failed, then blocked,
then unknown), so a campaign-level report and a delivery-level report never
disagree about the same facts.
"""

from __future__ import annotations

from collections.abc import Sequence

from pydantic import BaseModel, ConfigDict

from fno.company.contracts import EvidenceResult, NonEmptyStr


class _JoinModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class JoinBranch(_JoinModel):
    """A required campaign branch and its evidence result.

    The result is read only from EvidenceRef values bound to this branch's work
    order and attempt; an agent lifecycle, pane, claim release, transcript, or
    spawn receipt is never branch evidence.
    """

    work_order_id: NonEmptyStr
    attempt_id: NonEmptyStr
    result: EvidenceResult


class BranchRow(_JoinModel):
    """One branch's result as reported in a join evaluation."""

    work_order_id: NonEmptyStr
    attempt_id: NonEmptyStr
    result: EvidenceResult


class JoinEvaluation(_JoinModel):
    """The aggregate of a join plus every required branch row.

    Rows are never dropped: a blocked or failed join names which branch caused
    it. ``aggregate`` follows the failed>blocked>unknown precedence (empty ->
    unknown), mirroring the delivery evaluator so the two reports agree.
    """

    aggregate: EvidenceResult
    branches: tuple[BranchRow, ...]


_NON_PASSING_PRECEDENCE = (
    EvidenceResult.FAILED,
    EvidenceResult.BLOCKED,
    EvidenceResult.UNKNOWN,
)


def _aggregate(results: tuple[EvidenceResult, ...]) -> EvidenceResult:
    # Mirrors fno.delivery.evaluator._aggregate verbatim so a campaign join and
    # a delivery requirement row can never disagree about the same evidence.
    if results and all(result is EvidenceResult.PASSED for result in results):
        return EvidenceResult.PASSED
    for result in _NON_PASSING_PRECEDENCE:
        if result in results:
            return result
    return EvidenceResult.UNKNOWN


def evaluate_join(branches: Sequence[JoinBranch]) -> JoinEvaluation:
    """Evaluate a join over required branches.

    Passes only when every required branch is passed. Otherwise returns the
    non-passing aggregate using the failed>blocked>unknown precedence, and never
    drops a branch row.
    """
    rows = tuple(
        BranchRow(
            work_order_id=branch.work_order_id,
            attempt_id=branch.attempt_id,
            result=branch.result,
        )
        for branch in branches
    )
    return JoinEvaluation(
        aggregate=_aggregate(tuple(branch.result for branch in branches)),
        branches=rows,
    )


__all__ = [
    "BranchRow",
    "JoinBranch",
    "JoinEvaluation",
    "evaluate_join",
]
