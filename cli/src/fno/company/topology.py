"""Closed execution-topology vocabulary for company work.

Defines the four legal execution topologies a company work order may run under
and the validator a role manifest's ``default_topology`` must satisfy. The
vocabulary is the single serialization point shared with the function-pack
substrate: packaged role manifests emit only these four literals.
"""

from __future__ import annotations

from enum import Enum

from pydantic import BaseModel, ConfigDict, Field

from fno.company.contracts import NonEmptyStr
from fno.roles.models import RoleLayer


class _TopologyModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class Topology(str, Enum):
    """The complete and closed set of execution topologies.

    A fifth shape is a schema change, not a runtime string.
    """

    DIRECT = "direct"
    LOOP = "loop"
    SQUAD = "squad"
    PIPELINE = "pipeline"


class TopologyRefusal(_TopologyModel):
    """A typed refusal naming the offending topology value and how to fix it.

    Returned, never raised. ``value`` is a plain ``str`` (not ``NonEmptyStr``)
    so an offending input is recorded verbatim instead of rejecting the refusal
    itself; an empty or whitespace literal is a diagnosable authoring error.
    """

    value: str
    source_layer: RoleLayer | None = None
    recovery: NonEmptyStr


def validate_manifest_topology(
    value: str, *, source_layer: RoleLayer | None = None
) -> Topology | TopologyRefusal:
    """Validate a role manifest ``default_topology`` literal.

    Returns the matching :class:`Topology` for one of the four legal literals, or
    a :class:`TopologyRefusal` naming the offending value, its manifest source
    layer, and the recovery step. Never raises: ``RoleManifest.default_topology``
    is typed as a bare ``NonEmptyStr`` (``fno.roles.models``), so without this
    validator any string is a legal manifest and the failure surfaces at
    execution rather than at authoring.
    """
    try:
        return Topology(value)
    except ValueError:
        return TopologyRefusal(
            value=value,
            source_layer=source_layer,
            recovery="set default_topology to one of: direct, loop, squad, pipeline",
        )


class TopologySource(str, Enum):
    """Which precedence source decided a work order's topology."""

    PLAN = "plan"
    ROLE = "role"
    INFERENCE = "inference"


class InferenceFacts(_TopologyModel):
    """Work-order facts the closed inference table reads.

    ``has_declared_effect`` is carried because it is a present work-order fact,
    but it does not select a shape: an external effect routes to the approval
    boundary, not to execution topology, which keeps topology function-agnostic.
    """

    deliverable_count: int = Field(ge=0)
    has_dependency_edges: bool = False
    has_iteration_evaluator: bool = False
    has_declared_effect: bool = False


class TopologyResolution(_TopologyModel):
    """A resolved topology carrying only its shape and deciding source.

    Frozen with ``extra="forbid"`` so work-order identity, authority, and
    required evidence structurally cannot ride along: topology changes execution
    shape only.
    """

    shape: Topology
    source: TopologySource


def _infer_topology(facts: InferenceFacts) -> Topology:
    # Closed and total: every input combination yields exactly one shape, so
    # inference never returns nothing and consults no model. A wrong-but-
    # deterministic shape is always overridable by a plan lock.
    if facts.has_dependency_edges:
        return Topology.PIPELINE
    if facts.deliverable_count > 1:
        return Topology.SQUAD
    if facts.has_iteration_evaluator:
        return Topology.LOOP
    return Topology.DIRECT


def resolve_topology(
    *,
    plan_lock: str | None,
    role_default: str | None,
    role_source_layer: RoleLayer | None = None,
    inference_facts: InferenceFacts,
) -> TopologyResolution | TopologyRefusal:
    """Resolve one work order to one topology, reporting which source won.

    Precedence is fixed: a plan frontmatter ``topology:`` lock, then the
    resolved role's ``default_topology`` through
    :func:`validate_manifest_topology`, then closed runtime inference. The plan
    lock is a plan field, not a role overlay: the role overlay rule rejects any
    layer that changes ``default_topology`` (``fno.roles.resolver``), so a
    plan-layer role definition can never lock a topology.
    """
    if plan_lock is not None:
        try:
            shape = Topology(plan_lock)
        except ValueError:
            return TopologyRefusal(
                value=plan_lock,
                recovery=(
                    "set the plan frontmatter topology key to one of: "
                    "direct, loop, squad, pipeline"
                ),
            )
        return TopologyResolution(shape=shape, source=TopologySource.PLAN)

    if role_default is not None:
        validated = validate_manifest_topology(
            role_default, source_layer=role_source_layer
        )
        if isinstance(validated, TopologyRefusal):
            return validated
        return TopologyResolution(shape=validated, source=TopologySource.ROLE)

    return TopologyResolution(
        shape=_infer_topology(inference_facts), source=TopologySource.INFERENCE
    )


__all__ = [
    "InferenceFacts",
    "Topology",
    "TopologyRefusal",
    "TopologyResolution",
    "TopologySource",
    "resolve_topology",
    "validate_manifest_topology",
]
