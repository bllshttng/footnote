"""Project a resolved topology onto a work order without altering its truth.

The projection carries the resolved execution shape alongside the work order's
byte-identical identity, authority ceiling, approval floor, and required
evidence. It is a deliberate passthrough: topology changes execution shape only,
so a test can assert the four resolutions are field-identical on truth.
"""

from __future__ import annotations

from pydantic import BaseModel, ConfigDict

from fno.company.contracts import WorkOrderRef
from fno.company.topology import Topology, TopologyResolution, TopologySource
from fno.roles.models import ApprovalFloor, AuthorityCeiling


class _ExecutionModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class ExecutionProjection(_ExecutionModel):
    shape: Topology
    source: TopologySource
    work_order: WorkOrderRef
    authority_ceiling: AuthorityCeiling
    approval_floor: ApprovalFloor
    required_evidence_ids: tuple[str, ...]


def project_execution(
    *,
    resolution: TopologyResolution,
    work_order: WorkOrderRef,
    authority_ceiling: AuthorityCeiling,
    approval_floor: ApprovalFloor,
    required_evidence_ids: tuple[str, ...],
) -> ExecutionProjection:
    """Carry a resolved topology alongside the work order's unchanged truth.

    Identity, authority, approval floor, and required evidence pass through
    unchanged under every shape; the only field that varies is the shape (and
    its precedence source). This is the single seam the executor router uses, so
    no second router is needed.
    """
    return ExecutionProjection(
        shape=resolution.shape,
        source=resolution.source,
        work_order=work_order,
        authority_ceiling=authority_ceiling,
        approval_floor=approval_floor,
        required_evidence_ids=required_evidence_ids,
    )


__all__ = ["ExecutionProjection", "project_execution"]
