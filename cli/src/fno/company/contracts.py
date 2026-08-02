"""Immutable references that project company work onto Footnote's graph."""

from __future__ import annotations

from enum import Enum
from typing import Annotated, Self, TypeVar

from pydantic import BaseModel, ConfigDict, Field, StringConstraints, model_validator

NonEmptyStr = Annotated[str, StringConstraints(strip_whitespace=True, min_length=1)]
ContractRefT = TypeVar("ContractRefT")


class EvidenceResult(str, Enum):
    PASSED = "passed"
    FAILED = "failed"
    BLOCKED = "blocked"
    UNKNOWN = "unknown"


class EvidenceSubjectKind(str, Enum):
    ARTIFACT = "artifact"
    REVIEW = "review"
    APPROVAL = "approval"
    PROBE = "probe"
    ACKNOWLEDGMENT = "acknowledgment"
    DELIVERABLE = "deliverable"
    EFFECT = "effect"
    OBSERVATION = "observation"


class _ContractModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class PrincipalRef(_ContractModel):
    id: NonEmptyStr


class FunctionRef(_ContractModel):
    id: NonEmptyStr


class RoleRef(_ContractModel):
    id: NonEmptyStr
    function_id: NonEmptyStr


class WorkOrderRef(_ContractModel):
    node_id: NonEmptyStr
    attempt_id: NonEmptyStr
    principal_id: NonEmptyStr | None = None
    role_id: NonEmptyStr | None = None


class DeliverableRef(_ContractModel):
    id: NonEmptyStr
    kind: NonEmptyStr
    work_order_id: NonEmptyStr
    attempt_id: NonEmptyStr
    required_evidence_ids: tuple[NonEmptyStr, ...] = Field(default_factory=tuple)
    effect_id: NonEmptyStr | None = None


class EffectRef(_ContractModel):
    id: NonEmptyStr
    work_order_id: NonEmptyStr
    attempt_id: NonEmptyStr
    deliverable_id: NonEmptyStr | None = None
    effect_class: NonEmptyStr
    destination: NonEmptyStr
    idempotency_key: NonEmptyStr | None = None
    approval_id: NonEmptyStr | None = None


class EvidenceRef(_ContractModel):
    id: NonEmptyStr
    work_order_id: NonEmptyStr
    attempt_id: NonEmptyStr
    subject_kind: EvidenceSubjectKind
    subject_id: NonEmptyStr
    result: EvidenceResult


class ObservationRef(_ContractModel):
    node_id: NonEmptyStr
    delivery_work_order_id: NonEmptyStr
    deliverable_id: NonEmptyStr
    metric_ids: tuple[NonEmptyStr, ...] = Field(default_factory=tuple)


class CompanyWorkRefs(_ContractModel):
    """Optional typed projections; the graph remains work/lifecycle authority."""

    principal: PrincipalRef | None = None
    function: FunctionRef | None = None
    role: RoleRef | None = None
    work_order: WorkOrderRef | None = None
    deliverables: tuple[DeliverableRef, ...] = Field(default_factory=tuple)
    effects: tuple[EffectRef, ...] = Field(default_factory=tuple)
    evidence: tuple[EvidenceRef, ...] = Field(default_factory=tuple)
    observations: tuple[ObservationRef, ...] = Field(default_factory=tuple)

    @model_validator(mode="after")
    def _validate_references(self) -> Self:
        if self.role is not None and self.function is not None:
            if self.role.function_id != self.function.id:
                raise ValueError(
                    f"role {self.role.id} function_id does not match function {self.function.id}"
                )

        work_order = self.work_order
        dependent_refs = self.deliverables or self.effects or self.evidence or self.observations
        if dependent_refs and work_order is None:
            raise ValueError("delivery, effect, evidence, and observation refs require a work_order")
        if work_order is None:
            return self

        if self.principal is not None and work_order.principal_id is not None:
            if work_order.principal_id != self.principal.id:
                raise ValueError("work_order principal_id does not match principal")
        if self.role is not None and work_order.role_id is not None:
            if work_order.role_id != self.role.id:
                raise ValueError("work_order role_id does not match role")

        deliverables = self._unique_by_id("deliverable", self.deliverables)
        effects = self._unique_by_id("effect", self.effects)
        evidence = self._unique_by_id("evidence", self.evidence)
        observations = self._unique_observations(self.observations)

        for deliverable in self.deliverables:
            self._validate_attempt("deliverable", deliverable, work_order)
            missing_evidence = set(deliverable.required_evidence_ids) - evidence.keys()
            if missing_evidence:
                raise ValueError(
                    f"deliverable {deliverable.id} references undeclared evidence "
                    f"{sorted(missing_evidence)}"
                )
            if deliverable.effect_id is not None and deliverable.effect_id not in effects:
                raise ValueError(
                    f"deliverable {deliverable.id} references undeclared effect "
                    f"{deliverable.effect_id}"
                )

        for effect in self.effects:
            self._validate_attempt("effect", effect, work_order)
            if effect.deliverable_id is not None and effect.deliverable_id not in deliverables:
                raise ValueError(
                    f"effect {effect.id} references undeclared deliverable {effect.deliverable_id}"
                )

        for deliverable in self.deliverables:
            if deliverable.effect_id is None:
                continue
            effect = effects[deliverable.effect_id]
            if effect.deliverable_id is not None and effect.deliverable_id != deliverable.id:
                raise ValueError(
                    f"deliverable {deliverable.id} and effect {effect.id} have "
                    f"conflicting deliverable references"
                )

        for effect in self.effects:
            if effect.deliverable_id is None:
                continue
            deliverable = deliverables[effect.deliverable_id]
            if deliverable.effect_id is not None and deliverable.effect_id != effect.id:
                raise ValueError(
                    f"effect {effect.id} and deliverable {deliverable.id} have "
                    f"conflicting effect references"
                )

        for item in self.evidence:
            self._validate_attempt("evidence", item, work_order)
            declared_subjects: dict[EvidenceSubjectKind, set[str]] = {
                EvidenceSubjectKind.DELIVERABLE: set(deliverables),
                EvidenceSubjectKind.EFFECT: set(effects),
                EvidenceSubjectKind.OBSERVATION: set(observations),
            }
            subjects = declared_subjects.get(item.subject_kind)
            if subjects is not None and item.subject_id not in subjects:
                raise ValueError(
                    f"evidence {item.id} references undeclared "
                    f"{item.subject_kind.value} {item.subject_id}"
                )

        for observation in self.observations:
            if observation.delivery_work_order_id != work_order.node_id:
                raise ValueError(
                    f"observation {observation.node_id} delivery_work_order_id does not match "
                    f"work_order {work_order.node_id}"
                )
            if observation.node_id == work_order.node_id:
                raise ValueError("an observation must use a separate graph node from delivery work")
            if observation.deliverable_id not in deliverables:
                raise ValueError(
                    f"observation {observation.node_id} references undeclared deliverable "
                    f"{observation.deliverable_id}"
                )

        return self

    @staticmethod
    def _validate_attempt(kind: str, ref: object, work_order: WorkOrderRef) -> None:
        ref_id = getattr(ref, "id")
        if getattr(ref, "work_order_id") != work_order.node_id:
            raise ValueError(
                f"{kind} {ref_id} work_order_id does not match work_order {work_order.node_id}"
            )
        if getattr(ref, "attempt_id") != work_order.attempt_id:
            raise ValueError(
                f"{kind} {ref_id} attempt_id does not match work_order attempt_id"
            )

    @staticmethod
    def _unique_by_id(kind: str, refs: tuple[ContractRefT, ...]) -> dict[str, ContractRefT]:
        result: dict[str, ContractRefT] = {}
        for ref in refs:
            ref_id = getattr(ref, "id")
            if ref_id in result:
                raise ValueError(f"duplicate {kind} id {ref_id}")
            result[ref_id] = ref
        return result

    @staticmethod
    def _unique_observations(refs: tuple[ObservationRef, ...]) -> dict[str, ObservationRef]:
        result: dict[str, ObservationRef] = {}
        for ref in refs:
            if ref.node_id in result:
                raise ValueError(f"duplicate observation node_id {ref.node_id}")
            result[ref.node_id] = ref
        return result
