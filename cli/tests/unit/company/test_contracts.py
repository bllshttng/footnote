from __future__ import annotations

import pytest
from pydantic import ValidationError

from fno.company.contracts import (
    CompanyWorkRefs,
    DeliverableRef,
    EffectRef,
    EvidenceRef,
    EvidenceResult,
    FunctionRef,
    ObservationRef,
    PrincipalRef,
    RoleRef,
    WorkOrderRef,
)


def _company_work(**overrides: object) -> CompanyWorkRefs:
    values: dict[str, object] = {
        "principal": PrincipalRef(id="founder"),
        "function": FunctionRef(id="customer-success"),
        "role": RoleRef(id="customer-success-lead", function_id="customer-success"),
        "work_order": WorkOrderRef(
            node_id="x-e9a3",
            attempt_id="attempt-1",
            principal_id="founder",
            role_id="customer-success-lead",
        ),
        "deliverables": (
            DeliverableRef(
                id="response-draft",
                kind="internal-artifact",
                work_order_id="x-e9a3",
                attempt_id="attempt-1",
                required_evidence_ids=("evidence-draft",),
                effect_id="effect-send",
            ),
        ),
        "effects": (
            EffectRef(
                id="effect-send",
                work_order_id="x-e9a3",
                attempt_id="attempt-1",
                deliverable_id="response-draft",
                effect_class="external-communication",
                destination="helpdesk://ticket/42",
                idempotency_key="ticket-42-response-v1",
                approval_id="approval-42",
            ),
        ),
        "evidence": (
            EvidenceRef(
                id="evidence-draft",
                work_order_id="x-e9a3",
                attempt_id="attempt-1",
                subject_kind="deliverable",
                subject_id="response-draft",
                result=EvidenceResult.PASSED,
            ),
        ),
        "observations": (
            ObservationRef(
                node_id="x-observe",
                delivery_work_order_id="x-e9a3",
                deliverable_id="response-draft",
                metric_ids=("reply-rate",),
            ),
        ),
    }
    values.update(overrides)
    return CompanyWorkRefs(**values)


def test_ac1_hp_company_work_uses_graph_node_as_work_order_identity() -> None:
    refs = _company_work()

    assert refs.work_order is not None
    assert refs.work_order.node_id == "x-e9a3"
    assert refs.role is not None
    assert refs.role.function_id == "customer-success"


@pytest.mark.parametrize("result", ["passed", "failed", "blocked", "unknown"])
def test_ac7_hp_evidence_uses_honest_result_vocabulary(result: str) -> None:
    evidence = EvidenceRef(
        id="evidence-1",
        work_order_id="x-e9a3",
        attempt_id="attempt-1",
        subject_kind="probe",
        subject_id="probe-1",
        result=result,
    )

    assert evidence.result.value == result


def test_ac7_hp_evidence_rejects_an_unearned_result() -> None:
    with pytest.raises(ValidationError):
        EvidenceRef(
            id="evidence-1",
            work_order_id="x-e9a3",
            attempt_id="attempt-1",
            subject_kind="probe",
            subject_id="probe-1",
            result="succeeded",
        )


def test_ac7_hp_mismatched_delivery_attempt_is_rejected() -> None:
    deliverable = DeliverableRef(
        id="response-draft",
        kind="internal-artifact",
        work_order_id="x-e9a3",
        attempt_id="attempt-2",
    )

    with pytest.raises(ValidationError, match="deliverable response-draft attempt_id"):
        _company_work(deliverables=(deliverable,))


def test_ac7_hp_effect_cannot_reference_an_undeclared_deliverable() -> None:
    effect = EffectRef(
        id="effect-send",
        work_order_id="x-e9a3",
        attempt_id="attempt-1",
        deliverable_id="missing-deliverable",
        effect_class="external-communication",
        destination="helpdesk://ticket/42",
        idempotency_key="ticket-42-response-v1",
    )

    with pytest.raises(ValidationError, match="undeclared deliverable"):
        _company_work(effects=(effect,))


def test_ac7_hp_deliverable_and_effect_backlinks_must_agree() -> None:
    deliverables = (
        DeliverableRef(
            id="response-draft",
            kind="internal-artifact",
            work_order_id="x-e9a3",
            attempt_id="attempt-1",
            effect_id="effect-send",
        ),
        DeliverableRef(
            id="other-draft",
            kind="internal-artifact",
            work_order_id="x-e9a3",
            attempt_id="attempt-1",
        ),
    )
    effect = EffectRef(
        id="effect-send",
        work_order_id="x-e9a3",
        attempt_id="attempt-1",
        deliverable_id="other-draft",
        effect_class="external-communication",
        destination="helpdesk://ticket/42",
    )

    with pytest.raises(ValidationError, match="conflicting deliverable"):
        _company_work(deliverables=deliverables, effects=(effect,), evidence=())


def test_ac8_inv_observation_must_be_separate_graph_work() -> None:
    observation = ObservationRef(
        node_id="x-e9a3",
        delivery_work_order_id="x-e9a3",
        deliverable_id="response-draft",
        metric_ids=("reply-rate",),
    )

    with pytest.raises(ValidationError, match="separate graph node"):
        _company_work(observations=(observation,))


def test_ac11_journey_contracts_are_function_agnostic() -> None:
    refs = _company_work(
        function=FunctionRef(id="arbitrary-function"),
        role=RoleRef(id="arbitrary-owner", function_id="arbitrary-function"),
        work_order=WorkOrderRef(
            node_id="x-e9a3",
            attempt_id="attempt-1",
            principal_id="founder",
            role_id="arbitrary-owner",
        ),
    )

    assert refs.function is not None
    assert refs.function.id == "arbitrary-function"


def test_contracts_are_immutable() -> None:
    principal = PrincipalRef(id="founder")

    with pytest.raises(ValidationError, match="frozen"):
        principal.id = "someone-else"
