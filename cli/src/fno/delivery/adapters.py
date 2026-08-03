"""Pure shadow adapters for existing PR and research completion reads."""

from __future__ import annotations

from collections.abc import Mapping
from datetime import datetime
from typing import Annotated, Literal, Self

from pydantic import (
    AwareDatetime,
    BaseModel,
    ConfigDict,
    Field,
    StringConstraints,
    ValidationError,
    model_validator,
)

from fno.approvals import EffectAttempt, EffectState, evidence_projection
from fno.company.contracts import (
    CompanyWorkRefs,
    DeliverableRef,
    EffectRef,
    EvidenceRef,
    EvidenceResult,
    EvidenceSubjectKind,
    WorkOrderRef,
)
from fno.delivery.contracts import (
    DeliveryEvidenceFact,
    DeliveryEvidenceRejection,
    DeliveryEvidenceObservedEvent,
    DeliveryVerdict,
)
from fno.delivery.evaluator import evaluate_delivery

NonEmptyStr = Annotated[str, StringConstraints(strip_whitespace=True, min_length=1)]
LEGACY_PR_ADAPTER_VERSION = "legacy-pr-adapter.v1"
LEGACY_RESEARCH_ADAPTER_VERSION = "legacy-research-adapter.v1"
APPROVAL_EVENT_ADAPTER_VERSION = "approval-event-adapter.v1"
EFFECT_EVENT_ADAPTER_VERSION = "effect-event-adapter.v1"


class _ImmutableModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class _ProducerEvent(_ImmutableModel):
    ts: AwareDatetime
    type: Literal["approval_requested", "approval_decided", "effect_state_changed"]
    source: Literal["approvals"]
    data: dict[str, object]


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


def adapt_delivery_event(
    company_work: CompanyWorkRefs,
    event: Mapping[str, object] | object,
    *,
    fresh_until: datetime,
    fact_revision: str,
    approval_request_event: Mapping[str, object] | object | None = None,
) -> tuple[DeliveryEvidenceFact, ...]:
    """Normalize one authoritative event into its exact declared evidence slots.

    The adapter is a pure projection. It does not authorize a decision, advance
    an effect, or evaluate aggregate delivery. Unreadable, unknown, mismatched,
    or ambiguously bound inputs return no fact, which leaves coverage unknown.
    """
    if not isinstance(event, Mapping):
        return ()
    if event.get("type") == "delivery_evidence_observed":
        return _adapt_observed_event(company_work, event)
    try:
        producer_event = _ProducerEvent.model_validate(event)
    except ValidationError:
        return ()
    data = producer_event.data
    binding = _producer_binding(company_work, data)
    if binding is None:
        return ()
    event_type = producer_event.type
    if event_type in {"approval_requested", "approval_decided"}:
        request_digest = _nonempty(data, "request_digest")
        if request_digest is None or (
            binding.approval_id is not None and binding.approval_id != request_digest
        ):
            return ()
        declared = _match_slot(company_work, EvidenceSubjectKind.APPROVAL, request_digest)
        if declared is None:
            return ()
        if event_type == "approval_requested":
            if not _has_nonempty(
                data,
                "effect_class",
                "destination",
                "action_digest",
                "expires_at",
            ):
                return ()
            if (
                data["effect_class"] != binding.effect_class
                or data["destination"] != binding.destination
            ):
                return ()
            result = EvidenceResult.UNKNOWN
            source_revision = f"request:{request_digest}:requested"
        else:
            if not _has_nonempty(data, "deciding_principal_id"):
                return ()
            decision = _nonempty(data, "decision")
            if decision not in {"approved", "declined"}:
                return ()
            result = EvidenceResult.PASSED if decision == "approved" else EvidenceResult.BLOCKED
            source_revision = f"request:{request_digest}:decision:{decision}"
        adapter_version = APPROVAL_EVENT_ADAPTER_VERSION
    else:
        if not _has_nonempty(
            data,
            "idempotency_key",
            "state",
            "request_digest",
        ):
            return ()
        if (
            binding.idempotency_key is not None
            and data["idempotency_key"] != binding.idempotency_key
        ) or (binding.approval_id is not None and data["request_digest"] != binding.approval_id):
            return ()
        state = _nonempty(data, "state")
        previous_state = _nonempty(data, "previous_state")
        states = {state.value: state for state in EffectState}
        if (
            state not in states
            or (previous_state is None and state != EffectState.PREPARED.value)
            or (previous_state is not None and previous_state not in states)
        ):
            return ()
        if not isinstance(approval_request_event, Mapping):
            return ()
        try:
            request_event = _ProducerEvent.model_validate(approval_request_event)
        except ValidationError:
            return ()
        request_data = request_event.data
        if request_event.type != "approval_requested" or any(
            _nonempty(request_data, key) is None
            for key in (
                "request_digest",
                "work_order_id",
                "attempt_id",
                "effect_id",
                "effect_class",
                "destination",
                "action_digest",
            )
        ):
            return ()
        if any(
            request_data.get(key) != data.get(key)
            for key in ("request_digest", "work_order_id", "attempt_id", "effect_id")
        ):
            return ()
        if (
            request_data["effect_class"] != binding.effect_class
            or request_data["destination"] != binding.destination
        ):
            return ()
        request_digest = _nonempty(data, "request_digest")
        idempotency_key = _nonempty(data, "idempotency_key")
        action_digest = _nonempty(request_data, "action_digest")
        if request_digest is None or idempotency_key is None or action_digest is None:
            return ()
        attempt = EffectAttempt(
            effect_id=binding.id,
            work_order_id=binding.work_order_id,
            attempt_id=binding.attempt_id,
            request_digest=request_digest,
            idempotency_key=idempotency_key,
            action_digest=action_digest,
            destination=binding.destination,
            effect_class=binding.effect_class,
            adapter_id="event:approvals",
            adapter_version=EFFECT_EVENT_ADAPTER_VERSION,
            state=states[state],
            external_ref=_nonempty(data, "external_ref"),
            reconciliation_ref=_nonempty(data, "reconciliation_ref"),
        )
        projection = evidence_projection(attempt)
        if (
            projection.work_order_id != binding.work_order_id
            or projection.attempt_id != binding.attempt_id
            or projection.subject_kind is not EvidenceSubjectKind.EFFECT
            or projection.subject_id != binding.id
        ):
            return ()
        effect_id = binding.id
        result = projection.result
        key = _nonempty(data, "idempotency_key")
        source_revision = f"effect:{key}:state:{state}"
        adapter_version = EFFECT_EVENT_ADAPTER_VERSION
        kinds = [EvidenceSubjectKind.EFFECT]
        if state == "acknowledged":
            kinds.append(EvidenceSubjectKind.ACKNOWLEDGMENT)
        declared_slots = tuple(
            declared
            for kind in kinds
            if (declared := _match_slot(company_work, kind, effect_id)) is not None
        )
        return tuple(
            _event_fact(
                declared,
                result=result,
                producer_event=producer_event,
                source_revision=source_revision,
                fresh_until=fresh_until,
                adapter_version=adapter_version,
                fact_revision=fact_revision,
            )
            for declared in declared_slots
        )
    return (
        _event_fact(
            declared,
            result=result,
            producer_event=producer_event,
            source_revision=source_revision,
            fresh_until=fresh_until,
            adapter_version=adapter_version,
            fact_revision=fact_revision,
        ),
    )


def reject_delivery_event(
    company_work: CompanyWorkRefs,
    event: Mapping[str, object] | object,
    *,
    fact_revision: str,
) -> tuple[DeliveryEvidenceRejection, ...]:
    """Retain a rejected producer read when its requirement is identifiable."""
    if not isinstance(event, Mapping):
        return ()
    event_type = event.get("type")
    data = event.get("data")
    if not isinstance(event_type, str) or not isinstance(data, Mapping):
        return ()
    producer = _rejected_producer(event_type, data)
    diagnostics = _rejected_binding_diagnostics(company_work, event_type, event, data)
    if not diagnostics:
        return ()
    evidence_ids = _rejected_evidence_ids(company_work, event_type, data)
    return tuple(
        DeliveryEvidenceRejection(
            evidence_id=evidence_id,
            producer=producer,
            diagnostic=f"requirement {evidence_id} from {producer} rejected binding: "
            + ", ".join(diagnostics),
            fact_revision=fact_revision,
        )
        for evidence_id in evidence_ids
    )


def _rejected_producer(event_type: str, data: Mapping[str, object]) -> str:
    if event_type == "delivery_evidence_observed":
        producer = _nonempty(data, "producer")
        return producer or "unknown producer"
    source = "approvals" if event_type in {
        "approval_requested",
        "approval_decided",
        "effect_state_changed",
    } else "unknown"
    return f"event:{source}:{event_type}"


def _rejected_evidence_ids(
    company_work: CompanyWorkRefs,
    event_type: str,
    data: Mapping[str, object],
) -> tuple[str, ...]:
    required_ids = {
        evidence_id
        for deliverable in company_work.deliverables
        for evidence_id in deliverable.required_evidence_ids
    }
    if event_type == "delivery_evidence_observed":
        evidence = data.get("evidence")
        evidence_id = evidence.get("id") if isinstance(evidence, Mapping) else None
        return (evidence_id,) if isinstance(evidence_id, str) and evidence_id in required_ids else ()
    subject_ids: set[str]
    kinds: tuple[EvidenceSubjectKind, ...]
    if event_type in {"approval_requested", "approval_decided"}:
        request_digest = _nonempty(data, "request_digest")
        kinds = (EvidenceSubjectKind.APPROVAL,)
        binding = _producer_binding(company_work, data)
        subject_ids = {
            subject_id
            for subject_id in (request_digest, binding.approval_id if binding else None)
            if subject_id is not None
        }
    elif event_type == "effect_state_changed":
        effect_id = _nonempty(data, "effect_id")
        subject_ids = {effect_id} if effect_id is not None else set()
        kinds = (EvidenceSubjectKind.EFFECT, EvidenceSubjectKind.ACKNOWLEDGMENT)
    else:
        return ()
    if not subject_ids:
        return ()
    return tuple(
        evidence.id
        for evidence in company_work.evidence
        if evidence.id in required_ids
        and evidence.subject_kind in kinds
        and evidence.subject_id in subject_ids
    )


def _rejected_binding_diagnostics(
    company_work: CompanyWorkRefs,
    event_type: str,
    event: Mapping[str, object],
    data: Mapping[str, object],
) -> list[str]:
    diagnostics: list[str] = []
    if event_type == "delivery_evidence_observed":
        try:
            DeliveryEvidenceObservedEvent.model_validate(event)
        except ValidationError:
            diagnostics.append("malformed delivery_evidence_observed event")
        if event.get("source") != "target":
            diagnostics.append(f"source {event.get('source')}")
        return diagnostics
    if event.get("source") != "approvals":
        diagnostics.append(f"source {event.get('source')}")
    work_order = company_work.work_order
    if work_order is not None:
        if data.get("work_order_id") != work_order.node_id:
            diagnostics.append(f"work order {data.get('work_order_id')}")
        if data.get("attempt_id") != work_order.attempt_id:
            diagnostics.append(f"attempt {data.get('attempt_id')}")
    required = {
        "approval_requested": (
            "request_digest",
            "effect_id",
            "effect_class",
            "destination",
            "action_digest",
            "expires_at",
        ),
        "approval_decided": (
            "request_digest",
            "decision",
            "deciding_principal_id",
            "effect_id",
        ),
        "effect_state_changed": (
            "idempotency_key",
            "state",
            "previous_state",
            "request_digest",
            "effect_id",
        ),
    }.get(event_type, ())
    missing = [key for key in required if _nonempty(data, key) is None]
    if missing:
        diagnostics.append(f"missing {', '.join(missing)}")
    if not diagnostics:
        diagnostics.append("producer event did not match its declared slot")
    return diagnostics


def _adapt_observed_event(
    company_work: CompanyWorkRefs, event: Mapping[str, object]
) -> tuple[DeliveryEvidenceFact, ...]:
    if event.get("source") != "target":
        return ()
    try:
        fact = DeliveryEvidenceObservedEvent.model_validate(event).data
    except ValidationError:
        return ()
    declared = _match_slot(
        company_work,
        fact.evidence.subject_kind,
        fact.evidence.subject_id,
        evidence_id=fact.evidence.id,
    )
    work_order = company_work.work_order
    if declared is None or work_order is None:
        return ()
    if (
        fact.evidence.work_order_id != work_order.node_id
        or fact.evidence.attempt_id != work_order.attempt_id
    ):
        return ()
    return (fact,)


def _event_fact(
    declared: EvidenceRef,
    *,
    result: EvidenceResult,
    producer_event: _ProducerEvent,
    source_revision: str,
    fresh_until: datetime,
    adapter_version: str,
    fact_revision: str,
) -> DeliveryEvidenceFact:
    return DeliveryEvidenceFact(
        evidence=declared.model_copy(update={"result": result}),
        producer=f"event:approvals:{producer_event.type}",
        observed_at=producer_event.ts,
        source_revision=source_revision,
        fresh_until=fresh_until,
        adapter_version=adapter_version,
        fact_revision=fact_revision,
    )


def _producer_binding(
    company_work: CompanyWorkRefs, data: Mapping[str, object]
) -> EffectRef | None:
    work_order_id = _nonempty(data, "work_order_id")
    attempt_id = _nonempty(data, "attempt_id")
    effect_id = _nonempty(data, "effect_id")
    work_order = company_work.work_order
    if (
        work_order is None
        or work_order_id != work_order.node_id
        or attempt_id != work_order.attempt_id
        or effect_id is None
    ):
        return None
    effects = [effect for effect in company_work.effects if effect.id == effect_id]
    return effects[0] if len(effects) == 1 else None


def _match_slot(
    company_work: CompanyWorkRefs,
    subject_kind: EvidenceSubjectKind,
    subject_id: str,
    *,
    evidence_id: str | None = None,
) -> EvidenceRef | None:
    required_ids = {
        required_id
        for deliverable in company_work.deliverables
        for required_id in deliverable.required_evidence_ids
    }
    matches = [
        evidence
        for evidence in company_work.evidence
        if evidence.id in required_ids
        and (evidence_id is None or evidence.id == evidence_id)
        and evidence.subject_kind is subject_kind
        and evidence.subject_id == subject_id
    ]
    return matches[0] if len(matches) == 1 else None


def _nonempty(data: Mapping[str, object], key: str) -> str | None:
    value = data.get(key)
    return value if isinstance(value, str) and value.strip() else None


def _has_nonempty(data: Mapping[str, object], *keys: str) -> bool:
    return all(_nonempty(data, key) is not None for key in keys)


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


def adapt_legacy_research_grade(
    grade: object,
    *,
    work_order_node_id: str,
    attempt_id: str,
    artifact_read: bool,
    sidecar_read: bool,
    observed_at: datetime,
    fresh_until: datetime,
    source_revision: str,
    fact_revision: str,
    evaluated_at: datetime,
) -> LegacyDeliveryShadow:
    """Adapt the actual GradeResult/current setup reads without copying its verdict."""
    snapshot = LegacyResearchSnapshot(
        work_order_node_id=work_order_node_id,
        attempt_id=attempt_id,
        uncited_claims=getattr(grade, "uncited_claims"),
        dead_urls=getattr(grade, "dead_urls"),
        sections_uncovered=tuple(getattr(grade, "sections_uncovered")),
        artifact_read=artifact_read,
        sidecar_read=sidecar_read,
        observed_at=observed_at,
        fresh_until=fresh_until,
        source_revision=source_revision,
        fact_revision=fact_revision,
    )
    return adapt_legacy_research(snapshot, evaluated_at=evaluated_at)


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
