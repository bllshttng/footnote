from __future__ import annotations

import hashlib
from datetime import UTC, datetime

from fno.company.campaign import (
    BranchInput,
    CampaignProposal,
    CampaignRefusal,
    CampaignRefusalReason,
    PlannedDeliverable,
    PlannedEffect,
    classify_objective,
)
from fno.company.contracts import FunctionRef, RoleRef
from fno.company.topology import Topology
from fno.roles import (
    AuthorityCeiling,
    DeliveryPolicy,
    ReviewPolicy,
    RoleManifest,
)

NOW = datetime(2026, 8, 3, 12, tzinfo=UTC)


def _role(role_id: str, function_id: str) -> RoleManifest:
    return RoleManifest(
        role=RoleRef(id=role_id, function_id=function_id),
        function=FunctionRef(id=function_id),
        mission="Produce a bounded, reviewable artifact.",
        deliverable_kinds=("brief",),
        authority_ceiling=AuthorityCeiling.INTERNAL,
        review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
        delivery_policy=DeliveryPolicy(required_evidence=("artifact-exists",)),
        default_topology="direct",
    )


def _deliverable(
    did: str = "d-1", kind: str = "brief", *, effect: PlannedEffect | None = None
) -> PlannedDeliverable:
    return PlannedDeliverable(id=did, kind=kind, effect=effect)


def _branch(
    role_id: str,
    deliverables: tuple[PlannedDeliverable, ...],
    *,
    depends_on: tuple[str, ...] = (),
    requires_iteration: bool = False,
) -> BranchInput:
    return BranchInput(
        role_id=role_id,
        deliverables=deliverables,
        depends_on=depends_on,
        requires_iteration=requires_iteration,
    )


def test_ac2_two_branch_campaign_names_owners_deliverables_and_topology() -> None:
    proposal = classify_objective(
        objective="Grow the Q3 audience.",
        roles=[_role("role-a", "fn-a"), _role("role-b", "fn-b")],
        branches=[
            _branch("role-a", (_deliverable("d-1"),)),
            _branch("role-b", (_deliverable("d-2"),)),
        ],
        now=NOW,
    )
    assert isinstance(proposal, CampaignProposal)
    assert proposal.single_work_order is False
    assert proposal.topology is Topology.SQUAD
    assert {b.owner_function_id for b in proposal.branches} == {"fn-a", "fn-b"}
    assert {b.role_id for b in proposal.branches} == {"role-a", "role-b"}
    assert proposal.founder_decisions == ()
    assert proposal.created_at == NOW


def test_single_branch_is_a_single_work_order_with_direct_topology() -> None:
    proposal = classify_objective(
        objective="Ship one artifact.",
        roles=[_role("role-a", "fn-a")],
        branches=[_branch("role-a", (_deliverable("d-1"),))],
        now=NOW,
    )
    assert isinstance(proposal, CampaignProposal)
    assert proposal.single_work_order is True
    assert proposal.topology is Topology.DIRECT


def test_ordered_branches_infer_pipeline_topology() -> None:
    proposal = classify_objective(
        objective="Plan then publish.",
        roles=[_role("role-a", "fn-a"), _role("role-b", "fn-b")],
        branches=[
            _branch("role-a", (_deliverable("d-1"),)),
            _branch("role-b", (_deliverable("d-2"),), depends_on=("role-a",)),
        ],
        now=NOW,
    )
    assert isinstance(proposal, CampaignProposal)
    assert proposal.topology is Topology.PIPELINE


def test_iteration_inferred_as_loop_for_single_deliverable() -> None:
    proposal = classify_objective(
        objective="Refine the draft iteratively.",
        roles=[_role("role-a", "fn-a")],
        branches=[_branch("role-a", (_deliverable("d-1"),), requires_iteration=True)],
        now=NOW,
    )
    assert isinstance(proposal, CampaignProposal)
    assert proposal.topology is Topology.LOOP
    assert proposal.single_work_order is True


def test_declared_effect_surfaces_as_a_founder_decision() -> None:
    effect = PlannedEffect(effect_class="publish", destination="web")
    proposal = classify_objective(
        objective="Publish the post.",
        roles=[_role("role-a", "fn-a")],
        branches=[_branch("role-a", (_deliverable("d-1", effect=effect),))],
        now=NOW,
    )
    assert isinstance(proposal, CampaignProposal)
    assert proposal.founder_decisions == (effect,)
    # an effect does not select a topology shape
    assert proposal.topology is Topology.DIRECT


def test_ac2_classify_does_not_mutate_the_graph(tmp_path) -> None:
    graph = tmp_path / "graph.json"
    graph.write_text("{}")
    before = hashlib.sha256(graph.read_bytes()).hexdigest()
    proposal = classify_objective(
        objective="Pure objective.",
        roles=[_role("role-a", "fn-a")],
        branches=[_branch("role-a", (_deliverable("d-1"),))],
        now=NOW,
    )
    assert isinstance(proposal, CampaignProposal)
    after = hashlib.sha256(graph.read_bytes()).hexdigest()
    assert before == after


def test_refuses_empty_objective() -> None:
    refusal = classify_objective(
        objective="   ",
        roles=[_role("role-a", "fn-a")],
        branches=[_branch("role-a", (_deliverable("d-1"),))],
        now=NOW,
    )
    assert isinstance(refusal, CampaignRefusal)
    assert refusal.reason is CampaignRefusalReason.EMPTY_OBJECTIVE


def test_refuses_unresolvable_role_naming_it() -> None:
    refusal = classify_objective(
        objective="Objective.",
        roles=[_role("role-a", "fn-a")],
        branches=[_branch("role-missing", (_deliverable("d-1"),))],
        now=NOW,
    )
    assert isinstance(refusal, CampaignRefusal)
    assert refusal.reason is CampaignRefusalReason.NO_RESOLVABLE_ROLE
    assert refusal.detail == "role-missing"


def test_refuses_zero_deliverables() -> None:
    refusal = classify_objective(
        objective="Objective.",
        roles=[_role("role-a", "fn-a")],
        branches=[_branch("role-a", ())],
        now=NOW,
    )
    assert isinstance(refusal, CampaignRefusal)
    assert refusal.reason is CampaignRefusalReason.ZERO_DELIVERABLES
