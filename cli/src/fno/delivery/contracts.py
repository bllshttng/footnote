"""Versioned immutable contracts for runtime delivery evidence."""

from __future__ import annotations

from typing import Annotated, Literal, Self

from pydantic import AwareDatetime, BaseModel, ConfigDict, StringConstraints, model_validator

from fno.company.contracts import EvidenceRef, EvidenceResult, EvidenceSubjectKind

NonEmptyStr = Annotated[str, StringConstraints(strip_whitespace=True, min_length=1)]

DELIVERY_EVIDENCE_FACT_VERSION: Literal["delivery-evidence-fact.v1"] = "delivery-evidence-fact.v1"
DELIVERY_EVALUATOR_VERSION: Literal["delivery-evaluator.v1"] = "delivery-evaluator.v1"
DELIVERY_EVALUATE_RESPONSE_VERSION: Literal["delivery-evaluate-response.v1"] = (
    "delivery-evaluate-response.v1"
)


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


class DeliveryEvidenceRejection(_DeliveryModel):
    """A producer read that can be bound to a requirement but not accepted."""

    evidence_id: NonEmptyStr
    producer: NonEmptyStr
    diagnostic: NonEmptyStr
    fact_revision: NonEmptyStr


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


class DeliveryRequirementBinding(_DeliveryModel):
    """The declared identity of one required coverage row."""

    deliverable_id: NonEmptyStr
    evidence_id: NonEmptyStr


class DeliveryVerdict(_DeliveryModel):
    """Complete deterministic coverage for one work-order attempt."""

    evaluator_version: Literal["delivery-evaluator.v1"] = DELIVERY_EVALUATOR_VERSION
    session_id: NonEmptyStr | None = None
    work_order_node_id: NonEmptyStr
    attempt_id: NonEmptyStr
    aggregate: EvidenceResult
    fact_revision: NonEmptyStr | None = None
    required_requirements: tuple[DeliveryRequirementBinding, ...]
    requirements: tuple[DeliveryRequirementVerdict, ...]
    diagnostics: tuple[NonEmptyStr, ...] = ()

    @model_validator(mode="after")
    def _validate_complete_requirement_join(self) -> Self:
        declared = tuple(
            (item.deliverable_id, item.evidence_id) for item in self.required_requirements
        )
        evaluated = tuple(
            (item.deliverable_id, item.evidence_id) for item in self.requirements
        )
        if declared != evaluated:
            raise ValueError("requirements must exactly cover required_requirements")
        if len(set(declared)) != len(declared):
            raise ValueError("required_requirements must be unique")
        return self


class DeliveryEvidenceObservedEvent(_DeliveryModel):
    """Canonical event envelope for one already-produced runtime fact."""

    ts: AwareDatetime
    type: Literal["delivery_evidence_observed"]
    source: NonEmptyStr
    data: DeliveryEvidenceFact

    @model_validator(mode="after")
    def _validate_observation_time(self) -> Self:
        if self.ts != self.data.observed_at:
            raise ValueError("event ts must match evidence observed_at")
        return self


class DeliveryVerdictEvaluatedEvent(_DeliveryModel):
    """Canonical event envelope for the pure evaluator's derived result."""

    ts: AwareDatetime
    type: Literal["delivery_verdict_evaluated"]
    source: NonEmptyStr
    data: DeliveryVerdict


class DeliveryEvaluateResponse(_DeliveryModel):
    """Strict process boundary consumed by loop-check."""

    version: Literal["delivery-evaluate-response.v1"] = DELIVERY_EVALUATE_RESPONSE_VERSION
    status: Literal["inactive", "evaluated", "undeterminable"]
    fact_revision: NonEmptyStr | None = None
    verdict: DeliveryVerdict | None = None
    diagnostics: tuple[NonEmptyStr, ...] = ()

    @model_validator(mode="after")
    def _validate_status_payload(self) -> Self:
        if self.status == "evaluated":
            if self.verdict is None or self.fact_revision is None:
                raise ValueError("evaluated response requires verdict and fact_revision")
        elif self.verdict is not None:
            raise ValueError(f"{self.status} response must not carry a verdict")
        return self
