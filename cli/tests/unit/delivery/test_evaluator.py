from __future__ import annotations

from collections.abc import Callable
from datetime import datetime, timedelta, timezone

import pytest
from pydantic import ValidationError

from fno.company.contracts import (
    CompanyWorkRefs,
    DeliverableRef,
    EffectRef,
    EvidenceRef,
    EvidenceResult,
    EvidenceSubjectKind,
    FunctionRef,
    WorkOrderRef,
)
from fno.delivery.adapters import adapt_delivery_event
from fno.delivery import (
    DELIVERY_EVALUATOR_VERSION,
    DELIVERY_EVIDENCE_FACT_VERSION,
    DeliveryEvidenceFact,
    evaluate_delivery,
)
from fno.evals.research_grade import GradeResult

NOW = datetime(2026, 8, 2, 12, tzinfo=timezone.utc)
NODE_ID = "x-delivery"
ATTEMPT_ID = "attempt-1"
REQUIREMENTS = (
    ("artifact-ready", EvidenceSubjectKind.ARTIFACT, "artifact-1"),
    ("review-ready", EvidenceSubjectKind.REVIEW, "review-1"),
    ("approval-ready", EvidenceSubjectKind.APPROVAL, "approval-1"),
    ("probe-ready", EvidenceSubjectKind.PROBE, "probe-1"),
    ("ack-ready", EvidenceSubjectKind.ACKNOWLEDGMENT, "ack-1"),
)


def _company_work(function_name: str = "engineering") -> CompanyWorkRefs:
    return CompanyWorkRefs(
        function=FunctionRef(id=function_name),
        work_order=WorkOrderRef(node_id=NODE_ID, attempt_id=ATTEMPT_ID),
        deliverables=(
            DeliverableRef(
                id="release",
                kind="arbitrary-deliverable",
                work_order_id=NODE_ID,
                attempt_id=ATTEMPT_ID,
                required_evidence_ids=tuple(item[0] for item in REQUIREMENTS),
            ),
        ),
        evidence=tuple(
            EvidenceRef(
                id=evidence_id,
                work_order_id=NODE_ID,
                attempt_id=ATTEMPT_ID,
                subject_kind=subject_kind,
                subject_id=subject_id,
                result=EvidenceResult.PASSED,
            )
            for evidence_id, subject_kind, subject_id in REQUIREMENTS
        ),
    )


def _fact(
    evidence_id: str,
    *,
    result: EvidenceResult = EvidenceResult.PASSED,
    producer: str = "adapter:test",
    observed_at: datetime = NOW,
    fresh_until: datetime | None = None,
    attempt_id: str = ATTEMPT_ID,
    subject_kind: EvidenceSubjectKind | None = None,
    subject_id: str | None = None,
    fact_revision: str = "snapshot-7",
) -> DeliveryEvidenceFact:
    requirement = next(item for item in REQUIREMENTS if item[0] == evidence_id)
    return DeliveryEvidenceFact(
        version=DELIVERY_EVIDENCE_FACT_VERSION,
        evidence=EvidenceRef(
            id=evidence_id,
            work_order_id=NODE_ID,
            attempt_id=attempt_id,
            subject_kind=subject_kind or requirement[1],
            subject_id=subject_id or requirement[2],
            result=result,
        ),
        producer=producer,
        observed_at=observed_at,
        source_revision="source-sha-1",
        fresh_until=fresh_until or NOW + timedelta(minutes=5),
        adapter_version="test-adapter.v1",
        fact_revision=fact_revision,
    )


def _facts() -> tuple[DeliveryEvidenceFact, ...]:
    return tuple(_fact(item[0]) for item in REQUIREMENTS)


def test_ac_d1_hp_complete_five_slot_coverage_passes_in_declaration_order() -> None:
    verdict = evaluate_delivery(_company_work(), _facts(), evaluated_at=NOW)

    assert verdict.evaluator_version == DELIVERY_EVALUATOR_VERSION
    assert verdict.work_order_node_id == NODE_ID
    assert verdict.attempt_id == ATTEMPT_ID
    assert verdict.fact_revision == "snapshot-7"
    assert verdict.aggregate is EvidenceResult.PASSED
    assert [row.evidence_id for row in verdict.requirements] == [item[0] for item in REQUIREMENTS]
    assert {row.result for row in verdict.requirements} == {EvidenceResult.PASSED}


def test_ac_d1_err_projection_only_passed_values_are_not_runtime_evidence() -> None:
    verdict = evaluate_delivery(_company_work(), (), evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert all(row.result is EvidenceResult.UNKNOWN for row in verdict.requirements)
    assert "artifact-ready" in verdict.requirements[0].diagnostics[0]
    assert "missing runtime fact" in verdict.requirements[0].diagnostics[0]


@pytest.mark.parametrize(
    ("facts", "evidence_id", "diagnostic"),
    [
        (lambda: _facts()[1:], "artifact-ready", "missing runtime fact"),
        (
            lambda: (
                (
                    _fact(
                        "artifact-ready",
                        observed_at=NOW - timedelta(minutes=10),
                        fresh_until=NOW - timedelta(seconds=1),
                    ),
                )
                + _facts()[1:]
            ),
            "artifact-ready",
            "stale",
        ),
        (
            lambda: (_fact("artifact-ready", attempt_id="attempt-old"),) + _facts()[1:],
            "artifact-ready",
            "attempt-old",
        ),
        (
            lambda: (
                (
                    _fact("artifact-ready", producer="adapter:a"),
                    _fact(
                        "artifact-ready",
                        producer="adapter:b",
                        result=EvidenceResult.FAILED,
                    ),
                )
                + _facts()[1:]
            ),
            "artifact-ready",
            "adapter:a",
        ),
    ],
)
def test_ac_d2_err_missing_stale_mismatched_and_conflicting_facts_are_unknown(
    facts: Callable[[], tuple[DeliveryEvidenceFact, ...]],
    evidence_id: str,
    diagnostic: str,
) -> None:
    verdict = evaluate_delivery(_company_work(), facts(), evaluated_at=NOW)
    row = next(item for item in verdict.requirements if item.evidence_id == evidence_id)

    assert row.result is EvidenceResult.UNKNOWN
    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert evidence_id in " ".join(row.diagnostics)
    assert diagnostic in " ".join(row.diagnostics)


def test_ac_d2_err_malformed_and_unknown_version_facts_name_the_source() -> None:
    malformed = _fact("artifact-ready").model_dump(mode="json")
    malformed["version"] = "delivery-evidence-fact.v999"
    malformed["producer"] = "adapter:future"

    verdict = evaluate_delivery(_company_work(), (malformed,) + _facts()[1:], evaluated_at=NOW)

    row = verdict.requirements[0]
    assert row.result is EvidenceResult.UNKNOWN
    assert "artifact-ready" in " ".join(row.diagnostics)
    assert "adapter:future" in " ".join(row.diagnostics)
    assert "unknown version" in " ".join(row.diagnostics)


def test_ac_d3_err_precedence_is_failed_then_blocked_then_unknown_with_all_rows() -> None:
    facts = (
        _fact("artifact-ready", result=EvidenceResult.FAILED),
        _fact("review-ready", result=EvidenceResult.BLOCKED),
    )

    verdict = evaluate_delivery(_company_work(), facts, evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.FAILED
    assert [row.result for row in verdict.requirements] == [
        EvidenceResult.FAILED,
        EvidenceResult.BLOCKED,
        EvidenceResult.UNKNOWN,
        EvidenceResult.UNKNOWN,
        EvidenceResult.UNKNOWN,
    ]


def test_ac_d8_inv_observation_facts_are_excluded_and_irrelevant() -> None:
    passing = evaluate_delivery(_company_work(), _facts(), evaluated_at=NOW)
    observation = _fact(
        "artifact-ready",
        result=EvidenceResult.FAILED,
        producer="adapter:observation",
        subject_kind=EvidenceSubjectKind.OBSERVATION,
        subject_id="later-business-metric",
    )
    with_observation = evaluate_delivery(
        _company_work(), _facts() + (observation,), evaluated_at=NOW
    )

    assert passing == with_observation
    assert with_observation.aggregate is EvidenceResult.PASSED


@pytest.mark.parametrize(
    "function_name",
    [
        "marketing",
        "communications",
        "design",
        "social",
        "support",
        "operations",
        "sales",
        "arbitrary-unknown-function",
    ],
)
def test_ac_d9_inv_evaluation_is_function_agnostic(function_name: str) -> None:
    verdict = evaluate_delivery(_company_work(function_name), _facts(), evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.PASSED
    assert len(verdict.requirements) == len(REQUIREMENTS)


def test_ac_d10_con_mixed_fact_revisions_fail_closed() -> None:
    facts = (_fact("artifact-ready", fact_revision="snapshot-6"),) + _facts()[1:]

    verdict = evaluate_delivery(_company_work(), facts, evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert verdict.fact_revision is None
    assert all(row.result is EvidenceResult.UNKNOWN for row in verdict.requirements)
    assert "snapshot-6" in " ".join(verdict.diagnostics)
    assert "snapshot-7" in " ".join(verdict.diagnostics)


def test_contracts_are_immutable_and_versions_are_strict() -> None:
    fact = _fact("artifact-ready")

    with pytest.raises(ValidationError, match="frozen"):
        fact.producer = "adapter:other"
    with pytest.raises(ValidationError):
        DeliveryEvidenceFact(**{**fact.model_dump(), "version": "v2"})


def test_ac_d4_compat_pr_shadow_passes_only_with_all_five_legacy_reads() -> None:
    from fno.delivery import LegacyPRSnapshot, adapt_legacy_pr

    snapshot = LegacyPRSnapshot(
        work_order_node_id=NODE_ID,
        attempt_id=ATTEMPT_ID,
        pr_open=True,
        ci_ok=True,
        ci_pending=False,
        reviewed=True,
        head_shipped=True,
        probes_passed=True,
        current_head="head-sha",
        observed_at=NOW,
        fresh_until=NOW + timedelta(minutes=5),
        source_revision="pr-42:head-sha",
        fact_revision="pr-snapshot-1",
    )

    shadow = adapt_legacy_pr(snapshot, evaluated_at=NOW)

    assert shadow.legacy_passed is True
    assert shadow.verdict.aggregate is EvidenceResult.PASSED
    assert shadow.company_work.function is None
    assert [row.evidence_id for row in shadow.verdict.requirements] == [
        "legacy-pr-open",
        "legacy-pr-ci",
        "legacy-pr-review",
        "legacy-pr-head",
        "legacy-pr-probes",
    ]
    assert len(shadow.facts) == 5
    assert {fact.producer for fact in shadow.facts} == {"adapter:legacy-pr"}
    assert {fact.source_revision for fact in shadow.facts} == {"pr-42:head-sha"}
    assert {fact.fact_revision for fact in shadow.facts} == {"pr-snapshot-1"}
    assert {fact.observed_at for fact in shadow.facts} == {NOW}
    assert {fact.fresh_until for fact in shadow.facts} == {NOW + timedelta(minutes=5)}


@pytest.mark.parametrize(
    ("changes", "evidence_id", "expected"),
    [
        ({"pr_open": False}, "legacy-pr-open", EvidenceResult.FAILED),
        ({"pr_open": None}, "legacy-pr-open", EvidenceResult.UNKNOWN),
        ({"ci_ok": False}, "legacy-pr-ci", EvidenceResult.FAILED),
        ({"ci_ok": False, "ci_pending": True}, "legacy-pr-ci", EvidenceResult.BLOCKED),
        ({"ci_ok": None}, "legacy-pr-ci", EvidenceResult.UNKNOWN),
        ({"reviewed": False}, "legacy-pr-review", EvidenceResult.BLOCKED),
        ({"reviewed": None}, "legacy-pr-review", EvidenceResult.UNKNOWN),
        ({"head_shipped": False}, "legacy-pr-head", EvidenceResult.FAILED),
        ({"head_shipped": None}, "legacy-pr-head", EvidenceResult.UNKNOWN),
        ({"probes_passed": False}, "legacy-pr-probes", EvidenceResult.FAILED),
        ({"probes_passed": None}, "legacy-pr-probes", EvidenceResult.UNKNOWN),
    ],
)
def test_ac_d4_compat_pr_shadow_retains_each_legacy_blocker(
    changes: dict[str, bool | None], evidence_id: str, expected: EvidenceResult
) -> None:
    from fno.delivery import LegacyPRSnapshot, adapt_legacy_pr

    values: dict[str, object] = {
        "work_order_node_id": NODE_ID,
        "attempt_id": ATTEMPT_ID,
        "pr_open": True,
        "ci_ok": True,
        "ci_pending": False,
        "reviewed": True,
        "head_shipped": True,
        "probes_passed": True,
        "current_head": "head-sha",
        "observed_at": NOW,
        "fresh_until": NOW + timedelta(minutes=5),
        "source_revision": "pr-42:head-sha",
        "fact_revision": "pr-snapshot-1",
    }
    values.update(changes)

    shadow = adapt_legacy_pr(LegacyPRSnapshot(**values), evaluated_at=NOW)
    row = next(row for row in shadow.verdict.requirements if row.evidence_id == evidence_id)

    assert shadow.legacy_passed is False
    assert shadow.verdict.aggregate is not EvidenceResult.PASSED
    assert row.result is expected


def test_ac_d4_compat_research_shadow_preserves_exact_grade_and_setup_reads() -> None:
    from fno.delivery import LegacyResearchSnapshot, adapt_legacy_research

    snapshot = LegacyResearchSnapshot(
        work_order_node_id=NODE_ID,
        attempt_id=ATTEMPT_ID,
        uncited_claims=0,
        dead_urls=0,
        sections_uncovered=(),
        artifact_read=True,
        sidecar_read=True,
        observed_at=NOW,
        fresh_until=NOW + timedelta(minutes=5),
        source_revision="brief-sha:sidecar-sha",
        fact_revision="research-snapshot-1",
    )

    shadow = adapt_legacy_research(snapshot, evaluated_at=NOW)

    assert shadow.legacy_passed is True
    assert shadow.verdict.aggregate is EvidenceResult.PASSED
    assert [row.evidence_id for row in shadow.verdict.requirements] == [
        "legacy-research-artifact-read",
        "legacy-research-sidecar-read",
        "legacy-research-uncited-claims",
        "legacy-research-dead-urls",
        "legacy-research-section-coverage",
    ]


@pytest.mark.parametrize(
    ("grade_changes", "artifact_read", "sidecar_read"),
    [
        ({}, True, True),
        ({"uncited_claims": 1}, True, True),
        ({"dead_urls": 1}, True, True),
        ({"sections_uncovered": ["Findings"]}, True, True),
        ({}, False, True),
        ({}, True, False),
    ],
)
def test_ac_d4_compat_research_adapter_consumes_actual_grade_result_current_reads(
    grade_changes: dict[str, object], artifact_read: bool, sidecar_read: bool
) -> None:
    from fno.delivery import adapt_legacy_research_grade

    grade = GradeResult(brief="brief.md", golden="golden.md", sidecar="sources.jsonl")
    for field, value in grade_changes.items():
        setattr(grade, field, value)

    shadow = adapt_legacy_research_grade(
        grade,
        work_order_node_id=NODE_ID,
        attempt_id=ATTEMPT_ID,
        artifact_read=artifact_read,
        sidecar_read=sidecar_read,
        observed_at=NOW,
        fresh_until=NOW + timedelta(minutes=5),
        source_revision="brief-sha:sidecar-sha",
        fact_revision="research-snapshot-current",
        evaluated_at=NOW,
    )

    assert shadow.legacy_passed is (grade.green and artifact_read and sidecar_read)
    assert (shadow.verdict.aggregate is EvidenceResult.PASSED) is shadow.legacy_passed


@pytest.mark.parametrize(
    ("changes", "evidence_id", "expected"),
    [
        ({"uncited_claims": 1}, "legacy-research-uncited-claims", EvidenceResult.FAILED),
        ({"dead_urls": 1}, "legacy-research-dead-urls", EvidenceResult.FAILED),
        (
            {"sections_uncovered": ("Findings",)},
            "legacy-research-section-coverage",
            EvidenceResult.FAILED,
        ),
        ({"artifact_read": False}, "legacy-research-artifact-read", EvidenceResult.FAILED),
        ({"artifact_read": None}, "legacy-research-artifact-read", EvidenceResult.UNKNOWN),
        ({"sidecar_read": False}, "legacy-research-sidecar-read", EvidenceResult.FAILED),
        ({"sidecar_read": None}, "legacy-research-sidecar-read", EvidenceResult.UNKNOWN),
    ],
)
def test_ac_d4_compat_research_shadow_retains_grade_or_setup_failure(
    changes: dict[str, object], evidence_id: str, expected: EvidenceResult
) -> None:
    from fno.delivery import LegacyResearchSnapshot, adapt_legacy_research

    values: dict[str, object] = {
        "work_order_node_id": NODE_ID,
        "attempt_id": ATTEMPT_ID,
        "uncited_claims": 0,
        "dead_urls": 0,
        "sections_uncovered": (),
        "artifact_read": True,
        "sidecar_read": True,
        "observed_at": NOW,
        "fresh_until": NOW + timedelta(minutes=5),
        "source_revision": "brief-sha:sidecar-sha",
        "fact_revision": "research-snapshot-1",
    }
    values.update(changes)

    shadow = adapt_legacy_research(LegacyResearchSnapshot(**values), evaluated_at=NOW)
    row = next(row for row in shadow.verdict.requirements if row.evidence_id == evidence_id)

    assert shadow.legacy_passed is False
    assert shadow.verdict.aggregate is not EvidenceResult.PASSED
    assert row.result is expected


def test_ac_d4_compat_legacy_snapshots_and_shadow_results_are_immutable() -> None:
    from fno.delivery import LegacyPRSnapshot, adapt_legacy_pr

    snapshot = LegacyPRSnapshot(
        work_order_node_id=NODE_ID,
        attempt_id=ATTEMPT_ID,
        pr_open=True,
        ci_ok=True,
        ci_pending=False,
        reviewed=True,
        head_shipped=True,
        probes_passed=True,
        current_head="head-sha",
        observed_at=NOW,
        fresh_until=NOW + timedelta(minutes=5),
        source_revision="pr-42:head-sha",
        fact_revision="pr-snapshot-1",
    )
    shadow = adapt_legacy_pr(snapshot, evaluated_at=NOW)

    with pytest.raises(ValidationError, match="frozen"):
        snapshot.pr_open = False
    with pytest.raises(ValidationError, match="frozen"):
        shadow.legacy_passed = False


def _producer_company_work(*, duplicate_approval_slot: bool = False) -> CompanyWorkRefs:
    evidence = [
        EvidenceRef(
            id="approval-ready",
            work_order_id=NODE_ID,
            attempt_id=ATTEMPT_ID,
            subject_kind=EvidenceSubjectKind.APPROVAL,
            subject_id="request-digest-1",
            result=EvidenceResult.UNKNOWN,
        ),
        EvidenceRef(
            id="effect-acknowledged",
            work_order_id=NODE_ID,
            attempt_id=ATTEMPT_ID,
            subject_kind=EvidenceSubjectKind.EFFECT,
            subject_id="effect-1",
            result=EvidenceResult.UNKNOWN,
        ),
        EvidenceRef(
            id="destination-acknowledged",
            work_order_id=NODE_ID,
            attempt_id=ATTEMPT_ID,
            subject_kind=EvidenceSubjectKind.ACKNOWLEDGMENT,
            subject_id="effect-1",
            result=EvidenceResult.UNKNOWN,
        ),
    ]
    if duplicate_approval_slot:
        evidence.append(evidence[0].model_copy(update={"id": "approval-ready-duplicate"}))
    return CompanyWorkRefs(
        work_order=WorkOrderRef(node_id=NODE_ID, attempt_id=ATTEMPT_ID),
        deliverables=(
            DeliverableRef(
                id="external-send",
                kind="arbitrary-external-effect",
                work_order_id=NODE_ID,
                attempt_id=ATTEMPT_ID,
                required_evidence_ids=tuple(item.id for item in evidence),
                effect_id="effect-1",
            ),
        ),
        effects=(
            EffectRef(
                id="effect-1",
                work_order_id=NODE_ID,
                attempt_id=ATTEMPT_ID,
                deliverable_id="external-send",
                effect_class="communication.external",
                destination="email:customer@example.com",
                idempotency_key="effect-key-1",
                approval_id="request-digest-1",
            ),
        ),
        evidence=tuple(evidence),
    )


def _producer_event(event_type: str, **changes: object) -> dict[str, object]:
    data: dict[str, object]
    if event_type == "approval_requested":
        data = {
            "request_digest": "request-digest-1",
            "request_id": "request-1",
            "principal_id": "principal-1",
            "work_order_id": NODE_ID,
            "attempt_id": ATTEMPT_ID,
            "effect_id": "effect-1",
            "effect_class": "communication.external",
            "destination": "email:customer@example.com",
            "action_digest": "action-digest-1",
            "expires_at": "2026-08-02T13:00:00Z",
        }
    elif event_type == "approval_decided":
        data = {
            "request_digest": "request-digest-1",
            "decision": "approved",
            "deciding_principal_id": "principal-1",
            "work_order_id": NODE_ID,
            "attempt_id": ATTEMPT_ID,
            "effect_id": "effect-1",
        }
    else:
        data = {
            "idempotency_key": "effect-key-1",
            "state": "acknowledged",
            "previous_state": "executing",
            "request_digest": "request-digest-1",
            "work_order_id": NODE_ID,
            "attempt_id": ATTEMPT_ID,
            "effect_id": "effect-1",
            "external_ref": "message-42",
            "reconciliation_ref": None,
        }
    data.update(changes)
    return {
        "ts": NOW.isoformat().replace("+00:00", "Z"),
        "type": event_type,
        "source": "approvals",
        "data": data,
    }


@pytest.mark.parametrize(
    ("event_type", "changes", "expected"),
    [
        ("approval_requested", {}, {"approval-ready": EvidenceResult.UNKNOWN}),
        ("approval_decided", {}, {"approval-ready": EvidenceResult.PASSED}),
        (
            "approval_decided",
            {"decision": "declined"},
            {"approval-ready": EvidenceResult.BLOCKED},
        ),
        (
            "effect_state_changed",
            {},
            {
                "effect-acknowledged": EvidenceResult.PASSED,
                "destination-acknowledged": EvidenceResult.PASSED,
            },
        ),
        (
            "effect_state_changed",
            {"state": "failed"},
            {"effect-acknowledged": EvidenceResult.FAILED},
        ),
        (
            "effect_state_changed",
            {"state": "blocked"},
            {"effect-acknowledged": EvidenceResult.BLOCKED},
        ),
        (
            "effect_state_changed",
            {"state": "executing"},
            {"effect-acknowledged": EvidenceResult.UNKNOWN},
        ),
        (
            "effect_state_changed",
            {"state": "unknown"},
            {"effect-acknowledged": EvidenceResult.UNKNOWN},
        ),
    ],
)
def test_ac_d2_hp_producer_events_normalize_only_their_declared_source_fact(
    event_type: str,
    changes: dict[str, object],
    expected: dict[str, EvidenceResult],
) -> None:
    facts = adapt_delivery_event(
        _producer_company_work(),
        _producer_event(event_type, **changes),
        fresh_until=NOW + timedelta(minutes=5),
        fact_revision="event-snapshot-1",
        approval_request_event=(
            _producer_event("approval_requested")
            if event_type == "effect_state_changed"
            else None
        ),
    )

    assert {fact.evidence.id: fact.evidence.result for fact in facts} == expected
    assert {fact.evidence.work_order_id for fact in facts} == {NODE_ID}
    assert {fact.evidence.attempt_id for fact in facts} == {ATTEMPT_ID}
    assert {fact.producer for fact in facts} == {f"event:approvals:{event_type}"}
    assert {fact.fact_revision for fact in facts} == {"event-snapshot-1"}


def test_ac_d2_err_effect_event_without_request_metadata_is_not_evidence() -> None:
    assert (
        adapt_delivery_event(
            _producer_company_work(),
            _producer_event("effect_state_changed"),
            fresh_until=NOW + timedelta(minutes=5),
            fact_revision="event-snapshot-1",
        )
        == ()
    )


def test_ac_d2_hp_initial_prepared_effect_without_previous_state_stays_unknown() -> None:
    event = _producer_event("effect_state_changed", state="prepared")
    event["data"].pop("previous_state")

    facts = adapt_delivery_event(
        _producer_company_work(),
        event,
        fresh_until=NOW + timedelta(minutes=5),
        fact_revision="event-snapshot-1",
        approval_request_event=_producer_event("approval_requested"),
    )

    assert facts
    assert {fact.evidence.result for fact in facts} == {EvidenceResult.UNKNOWN}
    assert all(fact.evidence.result is not EvidenceResult.PASSED for fact in facts)


def test_ac_d6_inv_effect_normalization_uses_public_evidence_projection(monkeypatch) -> None:
    calls = []

    def project(attempt):
        from fno.approvals import evidence_projection as public_projection

        calls.append(attempt)
        return public_projection(attempt)

    monkeypatch.setattr("fno.delivery.adapters.evidence_projection", project)

    facts = adapt_delivery_event(
        _producer_company_work(),
        _producer_event("effect_state_changed"),
        fresh_until=NOW + timedelta(minutes=5),
        fact_revision="event-snapshot-1",
        approval_request_event=_producer_event("approval_requested"),
    )

    assert len(calls) == 1
    assert calls[0].adapter_id == "event:approvals"
    assert {fact.evidence.result for fact in facts} == {EvidenceResult.PASSED}


def test_ac_d2_edge_requested_approval_does_not_require_optional_identity_fields() -> None:
    event = _producer_event("approval_requested")
    assert isinstance(event["data"], dict)
    event["data"].pop("request_id")
    event["data"].pop("principal_id")

    facts = adapt_delivery_event(
        _producer_company_work(),
        event,
        fresh_until=NOW + timedelta(minutes=5),
        fact_revision="event-snapshot-1",
    )

    assert len(facts) == 1
    assert facts[0].evidence.result is EvidenceResult.UNKNOWN


@pytest.mark.parametrize(
    "event",
    [
        {"type": "approval_decided", "source": "approvals", "data": {}},
        _producer_event("approval_decided", decision="maybe"),
        _producer_event("effect_state_changed", state="delivered"),
        _producer_event("approval_decided", work_order_id="x-other"),
        _producer_event("approval_decided", attempt_id="attempt-old"),
        _producer_event("effect_state_changed", effect_id="effect-other"),
        _producer_event("approval_requested", destination="email:attacker@example.com"),
        _producer_event("effect_state_changed", idempotency_key="effect-key-other"),
        _producer_event("effect_state_changed", request_digest="request-digest-other"),
        {**_producer_event("approval_decided"), "source": "test"},
        {**_producer_event("approval_decided"), "type": "unknown_event"},
    ],
)
def test_ac_d2_err_malformed_unknown_and_mismatched_events_produce_no_fact(
    event: dict[str, object],
) -> None:
    assert (
        adapt_delivery_event(
            _producer_company_work(),
            event,
            fresh_until=NOW + timedelta(minutes=5),
            fact_revision="event-snapshot-1",
        )
        == ()
    )


def test_ac_d2_err_ambiguous_declared_slot_match_produces_no_fact() -> None:
    facts = adapt_delivery_event(
        _producer_company_work(duplicate_approval_slot=True),
        _producer_event("approval_decided"),
        fresh_until=NOW + timedelta(minutes=5),
        fact_revision="event-snapshot-1",
    )

    assert facts == ()


def test_ac_d3_err_conflicting_producer_events_never_resolve_by_last_writer() -> None:
    company_work = _producer_company_work()
    facts = tuple(
        fact
        for decision in ("approved", "declined")
        for fact in adapt_delivery_event(
            company_work,
            _producer_event("approval_decided", decision=decision),
            fresh_until=NOW + timedelta(minutes=5),
            fact_revision="event-snapshot-1",
        )
    )

    verdict = evaluate_delivery(company_work, facts, evaluated_at=NOW)

    approval = next(row for row in verdict.requirements if row.evidence_id == "approval-ready")
    assert approval.result is EvidenceResult.UNKNOWN
    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert "conflicting duplicate facts" in " ".join(approval.diagnostics)


def test_ac_d2_hp_delivery_evidence_observed_round_trips_a_declared_fact() -> None:
    declared = _producer_company_work()
    source_fact = DeliveryEvidenceFact(
        evidence=declared.evidence[1].model_copy(update={"result": EvidenceResult.PASSED}),
        producer="adapter:destination-api",
        observed_at=NOW,
        source_revision="message-42",
        fresh_until=NOW + timedelta(minutes=5),
        adapter_version="destination-api.v1",
        fact_revision="event-snapshot-1",
    )
    event = {
        "ts": "2026-08-02T12:00:00Z",
        "type": "delivery_evidence_observed",
        "source": "target",
        "data": source_fact.model_dump(mode="json"),
    }

    assert adapt_delivery_event(
        declared,
        event,
        fresh_until=NOW + timedelta(days=1),
        fact_revision="ignored-for-canonical-event",
    ) == (source_fact,)


@pytest.mark.parametrize(
    "change",
    [
        {"version": "delivery-evidence-fact.v999"},
        {"evidence": {"id": "effect-acknowledged"}},
    ],
)
def test_ac_d2_err_malformed_delivery_evidence_observed_produces_no_fact(
    change: dict[str, object],
) -> None:
    declared = _producer_company_work()
    source_fact = DeliveryEvidenceFact(
        evidence=declared.evidence[1],
        producer="adapter:destination-api",
        observed_at=NOW,
        source_revision="message-42",
        fresh_until=NOW + timedelta(minutes=5),
        adapter_version="destination-api.v1",
        fact_revision="event-snapshot-1",
    ).model_dump(mode="json")
    source_fact.update(change)

    assert (
        adapt_delivery_event(
            declared,
            {
                "ts": "2026-08-02T12:00:00Z",
                "type": "delivery_evidence_observed",
                "source": "target",
                "data": source_fact,
            },
            fresh_until=NOW + timedelta(minutes=5),
            fact_revision="event-snapshot-1",
        )
        == ()
    )
