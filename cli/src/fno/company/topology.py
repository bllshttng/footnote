"""Closed execution-topology vocabulary for company work.

Defines the four legal execution topologies a company work order may run under
and the validator a role manifest's ``default_topology`` must satisfy. The
vocabulary is the single serialization point shared with the function-pack
substrate: packaged role manifests emit only these four literals.
"""

from __future__ import annotations

from enum import Enum

from pydantic import BaseModel, ConfigDict

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


__all__ = ["Topology", "TopologyRefusal", "validate_manifest_topology"]
