"""Versioned immutable contracts for runtime delivery evidence."""

from __future__ import annotations

from typing import Annotated, Literal, Self

from pydantic import AwareDatetime, BaseModel, ConfigDict, StringConstraints, model_validator

from fno.company.contracts import EvidenceRef, EvidenceResult, EvidenceSubjectKind

NonEmptyStr = Annotated[str, StringConstraints(strip_whitespace=True, min_length=1)]

DELIVERY_EVIDENCE_FACT_VERSION = "delivery-evidence-fact.v1"
DELIVERY_EVALUATOR_VERSION = "delivery-evaluator.v1"


class _DeliveryModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class DeliveryEvidenceFact(_DeliveryModel):
    """One provenance-bearing runtime read from an evidence producer."""

    version: Literal["delivery-evidence-fact.v1"] = DELIVERY_EVIDENCE_FACT_VERSION
    evidence: EvidenceRef
    producer: NonEmptyStr
    observed_at: AwareDatetime
    source_revision: NonEmptyStr
    fresh_until: AwareDatetime
    adapter_version: NonEmptyStr
    fact_revision: NonEmptyStr

    @model_validator(mode="after")
    def _validate_freshness_window(self) -> Self:
        if self.fresh_until < self.observed_at:
            raise ValueError("fresh_until must not precede observed_at")
        return self


class DeliveryRequirementVerdict(_DeliveryModel):
    """The retained result for one declared deliverable requirement."""

    deliverable_id: NonEmptyStr
    evidence_id: NonEmptyStr
    subject_kind: EvidenceSubjectKind
    subject_id: NonEmptyStr
    result: EvidenceResult
    producers: tuple[NonEmptyStr, ...] = ()
    source_revisions: tuple[NonEmptyStr, ...] = ()
    diagnostics: tuple[NonEmptyStr, ...] = ()


class DeliveryVerdict(_DeliveryModel):
    """Complete deterministic coverage for one work-order attempt."""

    evaluator_version: Literal["delivery-evaluator.v1"] = DELIVERY_EVALUATOR_VERSION
    work_order_node_id: NonEmptyStr
    attempt_id: NonEmptyStr
    aggregate: EvidenceResult
    fact_revision: NonEmptyStr | None = None
    requirements: tuple[DeliveryRequirementVerdict, ...]
    diagnostics: tuple[NonEmptyStr, ...] = ()
