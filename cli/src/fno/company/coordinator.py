"""Campaign commit: the only writing path, plus the approval boundary.

``commit`` writes one epic and its child work orders through
``locked_mutate_graph`` (the sole graph mutator), which validates every
``company_work`` projection before graph bytes change. Cycle and epic-depth
safety reuse the existing guards; this module adds no second checker.

``request_effect_approval`` raises an ``ApprovalRequest`` through
``fno.approvals``, calls ``EffectStore.prepare``, and honors ``may_dispatch``.
It never injects an ``Authority``, never settles an attempt without its
dispatch token, and never reads the ``transport`` field for authorization.
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from pathlib import Path

from pydantic import BaseModel, ConfigDict

from fno.approvals.models import (
    AdapterCapability,
    ApprovalRequest,
    EffectDisposition,
    PrepareResult,
    RefusedError,
    classify_effect,
)
from fno.approvals.store import EffectStore
from fno.company.campaign import CampaignProposal
from fno.company.contracts import (
    CompanyWorkRefs,
    DeliverableRef,
    EffectRef,
    EvidenceRef,
    EvidenceResult,
    EvidenceSubjectKind,
    FunctionRef,
    RoleRef,
    WorkOrderRef,
)
from fno.graph._constants import mint_node_id
from fno.graph._intake import _would_create_cycle, _would_exceed_epic_depth
from fno.graph.store import locked_mutate_graph


class _CoordModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class CoordinatorRefusalReason(str, Enum):
    CYCLE = "cycle"
    EPIC_DEPTH = "epic_depth"
    DENIED_EFFECT = "denied_effect"
    BLOCKED_MISSING_APPROVAL = "blocked_missing_approval"


class CoordinatorRefusal(_CoordModel):
    reason: CoordinatorRefusalReason
    recovery: str
    detail: str | None = None


class CommittedChild(_CoordModel):
    node_id: str
    role_id: str
    owner_function_id: str
    depends_on: tuple[str, ...] = ()


class CommitResult(_CoordModel):
    epic_id: str
    children: tuple[CommittedChild, ...]


class _CommitAborted(Exception):
    """Raised inside the locked mutator to refuse without writing."""

    def __init__(self, refusal: CoordinatorRefusal) -> None:
        super().__init__(refusal.reason.value)
        self.refusal = refusal


def _node(
    *,
    node_id: str,
    parent: str | None,
    title: str,
    type_: str,
    project: str,
    now: datetime,
    blocked_by: list[str],
    company_work: dict | None = None,
) -> dict:
    node = {
        "id": node_id,
        "parent": parent,
        "title": title,
        "type": type_,
        "project": project,
        "cwd": None,
        "priority": "p2",
        "domain": "code",
        "blocked_by": list(blocked_by),
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "has_brief": False,
        "plan_path": None,
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "details": None,
        "size": None,
        "source": "coordinator",
        "created_at": now.isoformat(),
    }
    if company_work is not None:
        node["company_work"] = company_work
    return node


def _child_company_work(branch, child_id: str, attempt_id: str) -> dict:
    """Build a validated company_work projection for one child work order.

    Declares requirement rows only: evidence is declared ``unknown`` (never a
    result), because a plan-carried result declares projection shape only.
    """
    deliverables: list[DeliverableRef] = []
    effects: list[EffectRef] = []
    evidence: list[EvidenceRef] = []
    for planned in branch.deliverables:
        effect_id: str | None = None
        if planned.effect is not None:
            effect_id = f"{child_id}:effect:{planned.id}"
            effects.append(
                EffectRef(
                    id=effect_id,
                    work_order_id=child_id,
                    attempt_id=attempt_id,
                    deliverable_id=planned.id,
                    effect_class=planned.effect.effect_class,
                    destination=planned.effect.destination,
                    idempotency_key=f"{child_id}:{attempt_id}:{effect_id}",
                )
            )
        deliverables.append(
            DeliverableRef(
                id=planned.id,
                kind=planned.kind,
                work_order_id=child_id,
                attempt_id=attempt_id,
                required_evidence_ids=planned.required_evidence_ids,
                effect_id=effect_id,
            )
        )
        for evidence_id in planned.required_evidence_ids:
            evidence.append(
                EvidenceRef(
                    id=evidence_id,
                    work_order_id=child_id,
                    attempt_id=attempt_id,
                    subject_kind=EvidenceSubjectKind.DELIVERABLE,
                    subject_id=planned.id,
                    result=EvidenceResult.UNKNOWN,
                )
            )
    refs = CompanyWorkRefs(
        work_order=WorkOrderRef(
            node_id=child_id, attempt_id=attempt_id, role_id=branch.role_id
        ),
        function=FunctionRef(id=branch.owner_function_id),
        role=RoleRef(id=branch.role_id, function_id=branch.owner_function_id),
        deliverables=tuple(deliverables),
        effects=tuple(effects),
        evidence=tuple(evidence),
    )
    return refs.model_dump(mode="json", exclude_unset=True)


def commit(
    proposal: CampaignProposal,
    *,
    graph_path: Path,
    parent_id: str | None = None,
    project: str = "fno",
    now: datetime,
) -> CommitResult | CoordinatorRefusal:
    """Commit a proposal as one epic and N child work orders in the graph.

    Writes only through ``locked_mutate_graph`` (the sole graph mutator), which
    validates every ``company_work`` projection before bytes change. Cycle and
    epic-depth safety reuse the existing guards; this adds no second checker.
    Dependency edges between children are ordinary ``blocked_by`` edges, so the
    campaign is visible to ``fno backlog next`` and ``fno backlog advance`` with
    no changes to either.
    """
    epic_holder: list[str] = []
    children_holder: list[CommittedChild] = []

    def _mutate(entries: list[dict]) -> list[dict]:
        existing_ids = {
            e.get("id") for e in entries if isinstance(e, dict) and e.get("id")
        }

        epic_id = mint_node_id(existing_ids)
        existing_ids.add(epic_id)
        epic_node = _node(
            node_id=epic_id,
            parent=parent_id,
            title=proposal.objective,
            type_="epic",
            project=project,
            now=now,
            blocked_by=[],
        )

        if parent_id is not None:
            parent_node = next(
                (e for e in entries if e.get("id") == parent_id), None
            )
            if parent_node is not None and _would_exceed_epic_depth(
                entries, epic_node, parent_node
            ):
                raise _CommitAborted(
                    CoordinatorRefusal(
                        reason=CoordinatorRefusalReason.EPIC_DEPTH,
                        recovery="parent the campaign epic under a mission, not a nested epic",
                        detail=parent_id,
                    )
                )
            if _would_create_cycle(entries, epic_id, parent_id):
                raise _CommitAborted(
                    CoordinatorRefusal(
                        reason=CoordinatorRefusalReason.CYCLE,
                        recovery="parent the campaign under a node that is not its descendant",
                        detail=parent_id,
                    )
                )

        working = [*entries, epic_node]
        epic_holder.append(epic_id)

        # Mint every child id before mapping depends_on (role_id -> node id).
        role_to_node: dict[str, str] = {}
        for branch in proposal.branches:
            child_id = mint_node_id(existing_ids)
            existing_ids.add(child_id)
            role_to_node[branch.role_id] = child_id

        for branch in proposal.branches:
            child_id = role_to_node[branch.role_id]
            if _would_create_cycle(working, child_id, epic_id):
                raise _CommitAborted(
                    CoordinatorRefusal(
                        reason=CoordinatorRefusalReason.CYCLE,
                        recovery="a work order must not be its own ancestor",
                        detail=child_id,
                    )
                )
            depends_on = [
                role_to_node[dep]
                for dep in branch.depends_on
                if dep in role_to_node and role_to_node[dep] != child_id
            ]
            attempt_id = f"{child_id}#1"
            working.append(
                _node(
                    node_id=child_id,
                    parent=epic_id,
                    title=f"{proposal.objective}: {branch.role_id}",
                    type_="feature",
                    project=project,
                    now=now,
                    blocked_by=depends_on,
                    company_work=_child_company_work(branch, child_id, attempt_id),
                )
            )
            children_holder.append(
                CommittedChild(
                    node_id=child_id,
                    role_id=branch.role_id,
                    owner_function_id=branch.owner_function_id,
                    depends_on=tuple(depends_on),
                )
            )

        return working

    try:
        locked_mutate_graph(graph_path, _mutate)
    except _CommitAborted as aborted:
        return aborted.refusal

    return CommitResult(epic_id=epic_holder[0], children=tuple(children_holder))


def request_effect_approval(
    *,
    effect: EffectRef,
    principal_id: str,
    action_digest: str,
    request_id: str,
    created_at: datetime,
    expires_at: datetime,
    store: EffectStore,
    adapter: AdapterCapability,
) -> PrepareResult | CoordinatorRefusal:
    """Raise an approval request for a declared effect and honor ``may_dispatch``.

    A denied effect class is refused outright; listing a principal does not
    override the denial. Otherwise the request is submitted and the effect
    prepared through the store. The store consults its own authority at decision
    and execution time; this function never injects one, never settles an
    attempt it does not hold the dispatch token for, and never reads the
    ``transport`` field for authorization. A missing or non-approving decision
    returns a blocked refusal rather than dispatching.
    """
    if classify_effect(effect.effect_class) is EffectDisposition.DENY:
        return CoordinatorRefusal(
            reason=CoordinatorRefusalReason.DENIED_EFFECT,
            recovery="use a non-denied effect class; listing a principal does not override denial",
            detail=effect.effect_class,
        )

    request = ApprovalRequest(
        request_id=request_id,
        principal_id=principal_id,
        work_order_id=effect.work_order_id,
        attempt_id=effect.attempt_id,
        effect_id=effect.id,
        effect_class=effect.effect_class,
        destination=effect.destination,
        action_digest=action_digest,
        created_at=created_at,
        expires_at=expires_at,
    )
    store.submit(request)
    idempotency_key = effect.idempotency_key or (
        f"{effect.work_order_id}:{effect.attempt_id}:{effect.id}"
    )
    try:
        return store.prepare(
            request_digest=request.request_digest,
            idempotency_key=idempotency_key,
            adapter=adapter,
        )
    except RefusedError as exc:
        return CoordinatorRefusal(
            reason=CoordinatorRefusalReason.BLOCKED_MISSING_APPROVAL,
            recovery=(
                "approve the exact principal, action, destination, effect class, "
                "and attempt, then retry"
            ),
            detail=exc.refusal.reason.value,
        )


__all__ = [
    "CommitResult",
    "CommittedChild",
    "CoordinatorRefusal",
    "CoordinatorRefusalReason",
    "commit",
    "request_effect_approval",
]
