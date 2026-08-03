"""End-to-end: one objective becomes a committed campaign graph."""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

from fno.company.campaign import (
    BranchInput,
    PlannedDeliverable,
    PlannedEffect,
    classify_objective,
)
from fno.company.coordinator import CommitResult, commit
from fno.company.topology import Topology
from fno.company.contracts import FunctionRef, RoleRef
from fno.graph.store import read_graph
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
        mission="m",
        deliverable_kinds=("brief",),
        authority_ceiling=AuthorityCeiling.INTERNAL,
        review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
        delivery_policy=DeliveryPolicy(required_evidence=("artifact-exists",)),
        default_topology="direct",
    )


def test_objective_to_committed_campaign_graph(tmp_path: Path) -> None:
    effect = PlannedEffect(effect_class="publish", destination="web")
    proposal = classify_objective(
        objective="Launch the Q3 product narrative.",
        roles=[_role("role-plan", "fn-plan"), _role("role-publish", "fn-publish")],
        branches=[
            BranchInput(
                role_id="role-plan",
                deliverables=(PlannedDeliverable(id="d-plan", kind="brief"),),
            ),
            BranchInput(
                role_id="role-publish",
                deliverables=(
                    PlannedDeliverable(id="d-publish", kind="brief", effect=effect),
                ),
                depends_on=("role-plan",),
            ),
        ],
        now=NOW,
    )

    graph = tmp_path / "graph.json"
    result = commit(proposal, graph_path=graph, project="fno", now=NOW)

    assert isinstance(result, CommitResult)
    assert proposal.topology is Topology.PIPELINE
    assert len(result.children) == 2

    entries = {e["id"]: e for e in read_graph(graph)}
    epic = entries[result.epic_id]
    assert epic["type"] == "epic"
    # Both children are parented under the campaign epic with company_work bound
    # to their own node id, and the publish child carries a blocked_by edge and
    # a declared effect.
    planner = next(c for c in result.children if c.role_id == "role-plan")
    publisher = next(c for c in result.children if c.role_id == "role-publish")
    assert entries[publisher.node_id]["blocked_by"] == [planner.node_id]
    publish_cw = entries[publisher.node_id]["company_work"]
    assert publish_cw["work_order"]["node_id"] == publisher.node_id
    assert publish_cw["effects"]
    assert publish_cw["effects"][0]["effect_class"] == "publish"
