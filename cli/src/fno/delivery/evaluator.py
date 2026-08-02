"""Pure, function-agnostic evaluation of declared delivery evidence."""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from datetime import datetime

from pydantic import ValidationError

from fno.company.contracts import CompanyWorkRefs, EvidenceRef, EvidenceResult, EvidenceSubjectKind
from fno.delivery.contracts import (
    DELIVERY_EVALUATOR_VERSION,
    DeliveryEvidenceFact,
    DeliveryRequirementVerdict,
    DeliveryVerdict,
)


def evaluate_delivery(
    company_work: CompanyWorkRefs,
    facts: Iterable[DeliveryEvidenceFact | Mapping[str, object] | object],
    *,
    evaluated_at: datetime,
) -> DeliveryVerdict:
    """Evaluate all declared requirements against one runtime fact snapshot."""
    if evaluated_at.tzinfo is None or evaluated_at.utcoffset() is None:
        raise ValueError("evaluated_at must be timezone-aware")
    work_order = company_work.work_order
    if work_order is None:
        raise ValueError("delivery evaluation requires a work_order")

    declared = {evidence.id: evidence for evidence in company_work.evidence}
    requirements = tuple(
        (deliverable.id, evidence_id, declared[evidence_id])
        for deliverable in company_work.deliverables
        for evidence_id in deliverable.required_evidence_ids
    )
    required_ids = {evidence_id for _, evidence_id, _ in requirements}
    valid_by_id: dict[str, list[DeliveryEvidenceFact]] = {}
    rejected_by_id: dict[str, list[str]] = {}
    global_diagnostics: list[str] = []

    for raw_fact in facts:
        fact, evidence_id, diagnostic = _parse_fact(raw_fact)
        if fact is None:
            if evidence_id in required_ids:
                rejected_by_id.setdefault(evidence_id, []).append(diagnostic)
            elif evidence_id is None:
                global_diagnostics.append(diagnostic)
            continue
        if fact.evidence.subject_kind is EvidenceSubjectKind.OBSERVATION:
            continue
        if fact.evidence.id not in required_ids:
            continue
        valid_by_id.setdefault(fact.evidence.id, []).append(fact)

    revisions = sorted(
        {
            fact.fact_revision
            for evidence_id, evidence_facts in valid_by_id.items()
            if evidence_id in required_ids
            for fact in evidence_facts
        }
    )
    if len(revisions) > 1:
        diagnostic = f"mixed fact revisions: {', '.join(revisions)}"
        rows = tuple(
            _unknown_row(deliverable_id, evidence, diagnostic)
            for deliverable_id, _, evidence in requirements
        )
        return DeliveryVerdict(
            evaluator_version=DELIVERY_EVALUATOR_VERSION,
            work_order_node_id=work_order.node_id,
            attempt_id=work_order.attempt_id,
            aggregate=EvidenceResult.UNKNOWN,
            fact_revision=None,
            requirements=rows,
            diagnostics=tuple(global_diagnostics + [diagnostic]),
        )

    rows = tuple(
        _evaluate_requirement(
            deliverable_id=deliverable_id,
            declared=declared_evidence,
            facts=valid_by_id.get(evidence_id, []),
            rejected=rejected_by_id.get(evidence_id, []),
            work_order_node_id=work_order.node_id,
            attempt_id=work_order.attempt_id,
            evaluated_at=evaluated_at,
        )
        for deliverable_id, evidence_id, declared_evidence in requirements
    )
    if global_diagnostics:
        diagnostic = "; ".join(global_diagnostics)
        rows = tuple(
            row.model_copy(
                update={
                    "result": EvidenceResult.UNKNOWN,
                    "diagnostics": row.diagnostics + (diagnostic,),
                }
            )
            for row in rows
        )

    aggregate = _aggregate(tuple(row.result for row in rows))
    if not rows:
        aggregate = EvidenceResult.UNKNOWN
        global_diagnostics.append("no declared delivery evidence requirements")
    return DeliveryVerdict(
        evaluator_version=DELIVERY_EVALUATOR_VERSION,
        work_order_node_id=work_order.node_id,
        attempt_id=work_order.attempt_id,
        aggregate=aggregate,
        fact_revision=revisions[0] if len(revisions) == 1 else None,
        requirements=rows,
        diagnostics=tuple(global_diagnostics),
    )


def _parse_fact(
    value: DeliveryEvidenceFact | Mapping[str, object] | object,
) -> tuple[DeliveryEvidenceFact | None, str | None, str]:
    if isinstance(value, DeliveryEvidenceFact):
        return value, value.evidence.id, ""
    evidence_id, producer, version = _raw_identity(value)
    source = producer or "unknown producer"
    requirement = evidence_id or "unknown requirement"
    try:
        fact = DeliveryEvidenceFact.model_validate(value)
    except ValidationError:
        if version is not None and version != "delivery-evidence-fact.v1":
            reason = f"unknown version {version}"
        else:
            reason = "malformed or unreadable runtime fact"
        return None, evidence_id, f"requirement {requirement} from {source}: {reason}"
    return fact, fact.evidence.id, ""


def _raw_identity(value: object) -> tuple[str | None, str | None, str | None]:
    if not isinstance(value, Mapping):
        return None, None, None
    evidence = value.get("evidence")
    evidence_id = evidence.get("id") if isinstance(evidence, Mapping) else None
    producer = value.get("producer")
    version = value.get("version")
    return (
        evidence_id if isinstance(evidence_id, str) else None,
        producer if isinstance(producer, str) else None,
        version if isinstance(version, str) else None,
    )


def _evaluate_requirement(
    *,
    deliverable_id: str,
    declared: EvidenceRef,
    facts: list[DeliveryEvidenceFact],
    rejected: list[str],
    work_order_node_id: str,
    attempt_id: str,
    evaluated_at: datetime,
) -> DeliveryRequirementVerdict:
    diagnostics = list(rejected)
    candidates: list[DeliveryEvidenceFact] = []
    for fact in facts:
        evidence = fact.evidence
        binding_errors: list[str] = []
        if evidence.work_order_id != work_order_node_id:
            binding_errors.append(f"work order {evidence.work_order_id}")
        if evidence.attempt_id != attempt_id:
            binding_errors.append(f"attempt {evidence.attempt_id}")
        if evidence.subject_kind != declared.subject_kind:
            binding_errors.append(f"subject kind {evidence.subject_kind.value}")
        if evidence.subject_id != declared.subject_id:
            binding_errors.append(f"subject {evidence.subject_id}")
        if fact.observed_at > evaluated_at:
            binding_errors.append(f"future observation {fact.observed_at.isoformat()}")
        if fact.fresh_until < evaluated_at:
            binding_errors.append(f"stale after {fact.fresh_until.isoformat()}")
        if binding_errors:
            diagnostics.append(
                f"requirement {declared.id} from {fact.producer} rejected binding: "
                + ", ".join(binding_errors)
            )
        else:
            candidates.append(fact)

    if rejected or diagnostics:
        return _row(deliverable_id, declared, EvidenceResult.UNKNOWN, candidates, diagnostics)
    if not candidates:
        if declared.subject_kind is EvidenceSubjectKind.OBSERVATION:
            diagnostic = f"requirement {declared.id}: observation subjects cannot satisfy delivery"
        else:
            diagnostic = f"requirement {declared.id}: missing runtime fact"
        return _unknown_row(deliverable_id, declared, diagnostic)

    results = {fact.evidence.result for fact in candidates}
    bindings = {
        (
            fact.evidence.work_order_id,
            fact.evidence.attempt_id,
            fact.evidence.subject_kind,
            fact.evidence.subject_id,
        )
        for fact in candidates
    }
    if len(results) > 1 or len(bindings) > 1:
        sources = ", ".join(sorted({fact.producer for fact in candidates}))
        diagnostic = f"requirement {declared.id}: conflicting duplicate facts from {sources}"
        return _row(
            deliverable_id, declared, EvidenceResult.UNKNOWN, candidates, [diagnostic]
        )
    result = next(iter(results))
    diagnostics = []
    if result is EvidenceResult.UNKNOWN:
        diagnostics.append(f"requirement {declared.id} explicitly reported unknown")
    return _row(deliverable_id, declared, result, candidates, diagnostics)


def _unknown_row(
    deliverable_id: str, declared: EvidenceRef, diagnostic: str
) -> DeliveryRequirementVerdict:
    return _row(deliverable_id, declared, EvidenceResult.UNKNOWN, [], [diagnostic])


def _row(
    deliverable_id: str,
    declared: EvidenceRef,
    result: EvidenceResult,
    facts: list[DeliveryEvidenceFact],
    diagnostics: list[str],
) -> DeliveryRequirementVerdict:
    return DeliveryRequirementVerdict(
        deliverable_id=deliverable_id,
        evidence_id=declared.id,
        subject_kind=declared.subject_kind,
        subject_id=declared.subject_id,
        result=result,
        producers=tuple(sorted({fact.producer for fact in facts})),
        source_revisions=tuple(sorted({fact.source_revision for fact in facts})),
        diagnostics=tuple(diagnostics),
    )


def _aggregate(results: tuple[EvidenceResult, ...]) -> EvidenceResult:
    if results and all(result is EvidenceResult.PASSED for result in results):
        return EvidenceResult.PASSED
    for result in (EvidenceResult.FAILED, EvidenceResult.BLOCKED, EvidenceResult.UNKNOWN):
        if result in results:
            return result
    return EvidenceResult.UNKNOWN
