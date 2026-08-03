from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

from fno.approvals.models import (
    AdapterCapability,
    ApprovalRequest,
    DecisionKind,
    PrepareResult,
)
from fno.approvals.store import EffectStore
from fno.company.campaign import (
    BranchInput,
    PlannedDeliverable,
    PlannedEffect,
    classify_objective,
)
from fno.company.contracts import EffectRef, FunctionRef, RoleRef
from fno.company.coordinator import (
    CommitResult,
    CoordinatorRefusal,
    CoordinatorRefusalReason,
    commit,
    request_effect_approval,
)
from fno.graph.store import read_graph

NOW = datetime(2026, 8, 3, 12, tzinfo=UTC)


def _proposal(*, effect: PlannedEffect | None = None, depends: bool = False):
    from fno.roles import RoleManifest

    roles = [
        RoleManifest(
            role=RoleRef(id="role-a", function_id="fn-a"),
            function=FunctionRef(id="fn-a"),
            mission="m",
            deliverable_kinds=("brief",),
            authority_ceiling="internal",  # type: ignore[arg-type]
            review_policy={"required": True, "minimum_reviewers": 1},  # type: ignore[arg-type]
            delivery_policy={"required_evidence": ("artifact-exists",)},  # type: ignore[arg-type]
            default_topology="direct",
        ),
        RoleManifest(
            role=RoleRef(id="role-b", function_id="fn-b"),
            function=FunctionRef(id="fn-b"),
            mission="m",
            deliverable_kinds=("brief",),
            authority_ceiling="internal",  # type: ignore[arg-type]
            review_policy={"required": True, "minimum_reviewers": 1},  # type: ignore[arg-type]
            delivery_policy={"required_evidence": ("artifact-exists",)},  # type: ignore[arg-type]
            default_topology="direct",
        ),
    ]
    deliverable_a = PlannedDeliverable(id="d-a", kind="brief", effect=effect)
    deliverable_b = PlannedDeliverable(id="d-b", kind="brief")
    branches = [
        BranchInput(role_id="role-a", deliverables=(deliverable_a,)),
        BranchInput(
            role_id="role-b",
            deliverables=(deliverable_b,),
            depends_on=("role-a",) if depends else (),
        ),
    ]
    return classify_objective(
        objective="Grow the Q3 audience.",
        roles=roles,
        branches=branches,
        now=NOW,
    )


def test_ac1_commit_writes_one_epic_and_children_with_company_work(tmp_path: Path) -> None:
    proposal = _proposal(depends=True)
    graph = tmp_path / "graph.json"
    result = commit(proposal, graph_path=graph, project="fno", now=NOW)

    assert isinstance(result, CommitResult)
    assert len(result.children) == 2
    child_a = next(c for c in result.children if c.role_id == "role-a")
    child_b = next(c for c in result.children if c.role_id == "role-b")
    assert child_a.owner_function_id == "fn-a"
    assert child_b.owner_function_id == "fn-b"
    # role-b depends on role-a -> ordinary blocked_by edge to child_a's node.
    assert child_b.depends_on == (child_a.node_id,)

    entries = {e["id"]: e for e in read_graph(graph)}
    epic = entries[result.epic_id]
    assert epic["type"] == "epic"
    assert epic["parent"] is None

    for child in result.children:
        node = entries[child.node_id]
        assert node["parent"] == result.epic_id
        assert node["plan_path"] is None  # no plan file created
        cw = node["company_work"]
        assert cw["work_order"]["node_id"] == child.node_id
        assert cw["role"]["id"] == child.role_id
        assert cw["deliverables"]

    # The dependent child carries a blocked_by edge visible to backlog next.
    assert entries[child_b.node_id]["blocked_by"] == [child_a.node_id]


def test_ac1_commit_creates_no_file_outside_graph_and_kanban(tmp_path: Path) -> None:
    proposal = _proposal()
    graph = tmp_path / "graph.json"
    commit(proposal, graph_path=graph, project="fno", now=NOW)
    created = {p.name for p in tmp_path.iterdir()}
    # graph.json plus its Kanban projection; a backup/lock may also exist.
    assert "graph.json" in created
    assert all(
        name.startswith("graph") or name.endswith(".lock") or name.endswith(".bak")
        for name in created
    )


def test_ac1_refuses_when_parenting_under_a_nested_epic(tmp_path: Path) -> None:
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-epic0", "parent": None, "title": "e0", "type": "epic"},
                    {"id": "x-epic1", "parent": "x-epic0", "title": "e1", "type": "epic"},
                ]
            }
        )
    )
    proposal = _proposal()
    result = commit(proposal, graph_path=graph, parent_id="x-epic1", project="fno", now=NOW)
    assert isinstance(result, CoordinatorRefusal)
    assert result.reason is CoordinatorRefusalReason.EPIC_DEPTH
    # nothing was written: the seeded graph is intact
    seeded = {e["id"] for e in read_graph(graph)}
    assert seeded == {"x-epic0", "x-epic1"}


class _PermissiveAuthority:
    source = "test"

    def may_approve(self, *, principal_id: str, effect_class: str, destination: str) -> bool:
        return True


def _store(tmp_path: Path) -> EffectStore:
    return EffectStore(
        tmp_path / "approvals.db", authority=_PermissiveAuthority(), now=lambda: NOW
    )


def _effect(effect_class: str = "publish") -> EffectRef:
    return EffectRef(
        id="eff-1",
        work_order_id="wo-1",
        attempt_id="att-1",
        effect_class=effect_class,
        destination="web",
        idempotency_key="k1",
    )


def _adapter() -> AdapterCapability:
    return AdapterCapability(adapter_id="ad", adapter_version="1")


def test_ac7_denied_effect_class_refused(tmp_path: Path) -> None:
    store = _store(tmp_path)
    result = request_effect_approval(
        effect=_effect(effect_class="financial.payment"),
        principal_id="founder",
        action_digest="digest",
        request_id="req-1",
        created_at=NOW,
        expires_at=NOW + timedelta(hours=1),
        store=store,
        adapter=_adapter(),
    )
    assert isinstance(result, CoordinatorRefusal)
    assert result.reason is CoordinatorRefusalReason.DENIED_EFFECT


def test_ac7_unapproved_effect_is_blocked_not_dispatched(tmp_path: Path) -> None:
    store = _store(tmp_path)
    result = request_effect_approval(
        effect=_effect(),
        principal_id="founder",
        action_digest="digest",
        request_id="req-1",
        created_at=NOW,
        expires_at=NOW + timedelta(hours=1),
        store=store,
        adapter=_adapter(),
    )
    assert isinstance(result, CoordinatorRefusal)
    assert result.reason is CoordinatorRefusalReason.BLOCKED_MISSING_APPROVAL


def test_ac7_approved_effect_may_dispatch(tmp_path: Path) -> None:
    store = _store(tmp_path)
    effect = _effect()
    request = ApprovalRequest(
        request_id="req-1",
        principal_id="founder",
        work_order_id=effect.work_order_id,
        attempt_id=effect.attempt_id,
        effect_id=effect.id,
        effect_class=effect.effect_class,
        destination=effect.destination,
        action_digest="digest",
        created_at=NOW,
        expires_at=NOW + timedelta(hours=1),
    )
    store.submit(request)
    store.decide(
        request_digest=request.request_digest,
        deciding_principal_id="founder",
        decision=DecisionKind.APPROVED,
    )
    result = request_effect_approval(
        effect=effect,
        principal_id="founder",
        action_digest="digest",
        request_id="req-1",
        created_at=NOW,
        expires_at=NOW + timedelta(hours=1),
        store=store,
        adapter=_adapter(),
    )
    assert isinstance(result, PrepareResult)
    assert result.may_dispatch is True
    store.close()
