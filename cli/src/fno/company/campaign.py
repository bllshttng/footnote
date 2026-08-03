"""Pure objective classification into a non-mutating campaign proposal.

``classify_objective`` turns an objective, the available role definitions, and a
clock into a :class:`CampaignProposal` value and writes nothing. Splitting the
pure proposal from the mutating commit (``fno.company.coordinator``) is what
makes the proposal reviewable by the founder before anything durable happens
and what makes decomposition testable without a graph fixture.
"""

from __future__ import annotations

from collections.abc import Sequence
from datetime import datetime
from enum import Enum

from pydantic import BaseModel, ConfigDict

from fno.company.contracts import NonEmptyStr
from fno.company.topology import (
    InferenceFacts,
    Topology,
    TopologyResolution,
    resolve_topology,
)
from fno.roles.models import RoleManifest


class _CampaignModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class CampaignRefusalReason(str, Enum):
    EMPTY_OBJECTIVE = "empty_objective"
    NO_RESOLVABLE_ROLE = "no_resolvable_role"
    ZERO_DELIVERABLES = "zero_deliverables"


class PlannedEffect(_CampaignModel):
    """An external effect planned for a deliverable, requiring founder approval."""

    effect_class: NonEmptyStr
    destination: NonEmptyStr


class PlannedDeliverable(_CampaignModel):
    id: NonEmptyStr
    kind: NonEmptyStr
    required_evidence_ids: tuple[NonEmptyStr, ...] = ()
    effect: PlannedEffect | None = None


class BranchInput(_CampaignModel):
    """A proposed campaign branch before its role owner is resolved."""

    role_id: NonEmptyStr
    deliverables: tuple[PlannedDeliverable, ...]
    depends_on: tuple[NonEmptyStr, ...] = ()
    requires_iteration: bool = False


class ProposedBranch(_CampaignModel):
    """A campaign branch with its owning function resolved."""

    role_id: NonEmptyStr
    owner_function_id: NonEmptyStr
    deliverables: tuple[PlannedDeliverable, ...]
    depends_on: tuple[NonEmptyStr, ...] = ()
    requires_iteration: bool = False


class CampaignProposal(_CampaignModel):
    """An immutable, non-mutating proposal for a company objective.

    Names each role owner, its deliverables, the inferred execution topology,
    and the planned effects that require a founder decision. Nothing here is
    durable until ``coordinator.commit`` writes it through ``fno backlog``.
    """

    objective: NonEmptyStr
    branches: tuple[ProposedBranch, ...]
    topology: Topology
    founder_decisions: tuple[PlannedEffect, ...]
    created_at: datetime
    single_work_order: bool


class CampaignRefusal(_CampaignModel):
    """A typed refusal naming the reason and recovery step for a proposal."""

    reason: CampaignRefusalReason
    recovery: NonEmptyStr
    detail: NonEmptyStr | None = None


def classify_objective(
    *,
    objective: str,
    roles: Sequence[RoleManifest],
    branches: Sequence[BranchInput],
    now: datetime,
) -> CampaignProposal | CampaignRefusal:
    """Classify an objective into a pure, non-mutating :class:`CampaignProposal`.

    The branch structure is supplied input: the coordinator never invents
    function work (that would require a model, which is rejected), so it stays
    function-agnostic and deterministic. Returns a refusal for empty objective
    text, an unresolvable branch role, or zero deliverables; never raises.
    """
    if not objective.strip():
        return CampaignRefusal(
            reason=CampaignRefusalReason.EMPTY_OBJECTIVE,
            recovery="provide a non-empty company objective",
        )

    roles_by_id = {manifest.role.id: manifest for manifest in roles}
    for branch in branches:
        if branch.role_id not in roles_by_id:
            return CampaignRefusal(
                reason=CampaignRefusalReason.NO_RESOLVABLE_ROLE,
                recovery="provide role definitions for every branch role",
                detail=branch.role_id,
            )

    total_deliverables = sum(len(branch.deliverables) for branch in branches)
    if total_deliverables == 0:
        return CampaignRefusal(
            reason=CampaignRefusalReason.ZERO_DELIVERABLES,
            recovery="declare at least one deliverable on a branch",
        )

    proposed = tuple(
        ProposedBranch(
            role_id=branch.role_id,
            owner_function_id=roles_by_id[branch.role_id].function.id,
            deliverables=branch.deliverables,
            depends_on=branch.depends_on,
            requires_iteration=branch.requires_iteration,
        )
        for branch in branches
    )

    founder_decisions = tuple(
        deliverable.effect
        for branch in proposed
        for deliverable in branch.deliverables
        if deliverable.effect is not None
    )

    resolution = resolve_topology(
        plan_lock=None,
        role_default=None,
        inference_facts=InferenceFacts(
            deliverable_count=total_deliverables,
            has_dependency_edges=any(branch.depends_on for branch in branches),
            has_iteration_evaluator=any(
                branch.requires_iteration for branch in branches
            ),
            has_declared_effect=bool(founder_decisions),
        ),
    )
    # plan_lock and role_default are both None, so inference always resolves.
    assert isinstance(resolution, TopologyResolution)

    return CampaignProposal(
        objective=objective,
        branches=proposed,
        topology=resolution.shape,
        founder_decisions=founder_decisions,
        created_at=now,
        single_work_order=len(branches) <= 1,
    )


__all__ = [
    "BranchInput",
    "CampaignProposal",
    "CampaignRefusal",
    "CampaignRefusalReason",
    "PlannedDeliverable",
    "PlannedEffect",
    "ProposedBranch",
    "classify_objective",
]
