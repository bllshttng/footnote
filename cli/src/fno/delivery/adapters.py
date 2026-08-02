"""Pure shadow adapters for existing PR and research completion reads."""

from __future__ import annotations

from datetime import datetime
from typing import Annotated, Self

from pydantic import AwareDatetime, BaseModel, ConfigDict, Field, StringConstraints, model_validator

from fno.company.contracts import (
    CompanyWorkRefs,
    DeliverableRef,
    EvidenceRef,
    EvidenceResult,
    EvidenceSubjectKind,
    WorkOrderRef,
)
from fno.delivery.contracts import DeliveryEvidenceFact, DeliveryVerdict
from fno.delivery.evaluator import evaluate_delivery

NonEmptyStr = Annotated[str, StringConstraints(strip_whitespace=True, min_length=1)]
LEGACY_PR_ADAPTER_VERSION = "legacy-pr-adapter.v1"
LEGACY_RESEARCH_ADAPTER_VERSION = "legacy-research-adapter.v1"


class _ImmutableModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class _LegacySnapshot(_ImmutableModel):
    work_order_node_id: NonEmptyStr
    attempt_id: NonEmptyStr
    observed_at: AwareDatetime
    fresh_until: AwareDatetime
    source_revision: NonEmptyStr
    fact_revision: NonEmptyStr

    @model_validator(mode="after")
    def _validate_freshness_window(self) -> Self:
        if self.fresh_until < self.observed_at:
            raise ValueError("fresh_until must not precede observed_at")
        return self


class LegacyPRSnapshot(_LegacySnapshot):
    """Already-read values used by the current DonePRGreen conjunction."""

    pr_open: bool | None
    ci_ok: bool | None
    ci_pending: bool = False
    reviewed: bool | None
    head_shipped: bool | None
    probes_passed: bool | None
    current_head: NonEmptyStr

    @model_validator(mode="after")
    def _validate_ci_state(self) -> Self:
        if self.ci_pending and self.ci_ok is not False:
            raise ValueError("ci_pending requires ci_ok false")
        return self


class LegacyResearchSnapshot(_LegacySnapshot):
    """Already-read values used by GradeResult.green and delivery setup."""

    uncited_claims: int | None = Field(ge=0)
    dead_urls: int | None = Field(ge=0)
    sections_uncovered: tuple[NonEmptyStr, ...] | None
    artifact_read: bool | None
    sidecar_read: bool | None


class LegacyDeliveryShadow(_ImmutableModel):
    """Legacy decision plus the canonical evidence and verdict observed in shadow."""

    legacy_passed: bool
    company_work: CompanyWorkRefs
    facts: tuple[DeliveryEvidenceFact, ...]
    verdict: DeliveryVerdict


def adapt_legacy_pr(snapshot: LegacyPRSnapshot, *, evaluated_at: datetime) -> LegacyDeliveryShadow:
    """Normalize existing PR reads without changing the legacy completion path."""
    results = (
        ("legacy-pr-open", EvidenceSubjectKind.ARTIFACT, "pull-request", _result(snapshot.pr_open)),
        (
            "legacy-pr-ci",
            EvidenceSubjectKind.PROBE,
            "continuous-integration",
            EvidenceResult.BLOCKED if snapshot.ci_pending else _result(snapshot.ci_ok),
        ),
        (
            "legacy-pr-review",
            EvidenceSubjectKind.REVIEW,
            "required-review",
            _result(snapshot.reviewed, false=EvidenceResult.BLOCKED),
        ),
        (
            "legacy-pr-head",
            EvidenceSubjectKind.ARTIFACT,
            f"head:{snapshot.current_head}",
            _result(snapshot.head_shipped),
        ),
        (
            "legacy-pr-probes",
            EvidenceSubjectKind.PROBE,
            "done-probes",
            _result(snapshot.probes_passed),
        ),
    )
    legacy_passed = (
        snapshot.pr_open is True
        and snapshot.ci_ok is True
        and not snapshot.ci_pending
        and snapshot.reviewed is True
        and snapshot.head_shipped is True
        and snapshot.probes_passed is True
    )
    return _evaluate_shadow(
        snapshot,
        deliverable_kind="legacy-pr",
        adapter_version=LEGACY_PR_ADAPTER_VERSION,
        producer="adapter:legacy-pr",
        results=results,
        legacy_passed=legacy_passed,
        evaluated_at=evaluated_at,
    )


def adapt_legacy_research(
    snapshot: LegacyResearchSnapshot, *, evaluated_at: datetime
) -> LegacyDeliveryShadow:
    """Normalize GradeResult.green inputs and its required setup reads."""
    results = (
        (
            "legacy-research-artifact-read",
            EvidenceSubjectKind.ARTIFACT,
            "research-artifact",
            _result(snapshot.artifact_read),
        ),
        (
            "legacy-research-sidecar-read",
            EvidenceSubjectKind.ARTIFACT,
            "research-sidecar",
            _result(snapshot.sidecar_read),
        ),
        (
            "legacy-research-uncited-claims",
            EvidenceSubjectKind.PROBE,
            "uncited-claims-zero",
            _zero_result(snapshot.uncited_claims),
        ),
        (
            "legacy-research-dead-urls",
            EvidenceSubjectKind.PROBE,
            "dead-urls-zero",
            _zero_result(snapshot.dead_urls),
        ),
        (
            "legacy-research-section-coverage",
            EvidenceSubjectKind.PROBE,
            "sections-uncovered-empty",
            _empty_result(snapshot.sections_uncovered),
        ),
    )
    legacy_passed = (
        snapshot.artifact_read is True
        and snapshot.sidecar_read is True
        and snapshot.uncited_claims == 0
        and snapshot.dead_urls == 0
        and snapshot.sections_uncovered == ()
    )
    return _evaluate_shadow(
        snapshot,
        deliverable_kind="legacy-research",
        adapter_version=LEGACY_RESEARCH_ADAPTER_VERSION,
        producer="adapter:legacy-research",
        results=results,
        legacy_passed=legacy_passed,
        evaluated_at=evaluated_at,
    )


def _evaluate_shadow(
    snapshot: _LegacySnapshot,
    *,
    deliverable_kind: str,
    adapter_version: str,
    producer: str,
    results: tuple[tuple[str, EvidenceSubjectKind, str, EvidenceResult], ...],
    legacy_passed: bool,
    evaluated_at: datetime,
) -> LegacyDeliveryShadow:
    work_order = WorkOrderRef(
        node_id=snapshot.work_order_node_id,
        attempt_id=snapshot.attempt_id,
    )
    evidence = tuple(
        EvidenceRef(
            id=evidence_id,
            work_order_id=work_order.node_id,
            attempt_id=work_order.attempt_id,
            subject_kind=subject_kind,
            subject_id=subject_id,
            result=result,
        )
        for evidence_id, subject_kind, subject_id, result in results
    )
    company_work = CompanyWorkRefs(
        work_order=work_order,
        deliverables=(
            DeliverableRef(
                id="legacy-deliverable",
                kind=deliverable_kind,
                work_order_id=work_order.node_id,
                attempt_id=work_order.attempt_id,
                required_evidence_ids=tuple(item.id for item in evidence),
            ),
        ),
        evidence=evidence,
    )
    facts = tuple(
        DeliveryEvidenceFact(
            evidence=item,
            producer=producer,
            observed_at=snapshot.observed_at,
            source_revision=snapshot.source_revision,
            fresh_until=snapshot.fresh_until,
            adapter_version=adapter_version,
            fact_revision=snapshot.fact_revision,
        )
        for item in evidence
    )
    verdict = evaluate_delivery(company_work, facts, evaluated_at=evaluated_at)
    return LegacyDeliveryShadow(
        legacy_passed=legacy_passed,
        company_work=company_work,
        facts=facts,
        verdict=verdict,
    )


def _result(value: bool | None, *, false: EvidenceResult = EvidenceResult.FAILED) -> EvidenceResult:
    if value is True:
        return EvidenceResult.PASSED
    if value is False:
        return false
    return EvidenceResult.UNKNOWN


def _zero_result(value: int | None) -> EvidenceResult:
    if value is None:
        return EvidenceResult.UNKNOWN
    return EvidenceResult.PASSED if value == 0 else EvidenceResult.FAILED


def _empty_result(value: tuple[str, ...] | None) -> EvidenceResult:
    if value is None:
        return EvidenceResult.UNKNOWN
    return EvidenceResult.PASSED if not value else EvidenceResult.FAILED
