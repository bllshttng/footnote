from __future__ import annotations

from fno.company.contracts import WorkOrderRef
from fno.company.execution import project_execution
from fno.company.topology import Topology, TopologyResolution, TopologySource
from fno.roles.models import ApprovalFloor, AuthorityCeiling

WORK_ORDER = WorkOrderRef(node_id="x-1", attempt_id="a-1", role_id="role-a")
AUTHORITY = AuthorityCeiling.EXTERNAL
APPROVAL = ApprovalFloor.FOUNDER
EVIDENCE = ("ev-1", "ev-2")

_TRUTH_FIELDS = ("work_order", "authority_ceiling", "approval_floor", "required_evidence_ids")


def _project(shape: Topology):
    return project_execution(
        resolution=TopologyResolution(shape=shape, source=TopologySource.INFERENCE),
        work_order=WORK_ORDER,
        authority_ceiling=AUTHORITY,
        approval_floor=APPROVAL,
        required_evidence_ids=EVIDENCE,
    )


def test_ac3_truth_is_identical_across_all_four_shapes() -> None:
    projections = {shape: _project(shape) for shape in Topology}
    for field in _TRUTH_FIELDS:
        first = getattr(projections[Topology.DIRECT], field)
        for shape, projection in projections.items():
            assert getattr(projection, field) == first, f"{field} differs under {shape.value}"


def test_ac3_only_shape_varies_across_resolutions() -> None:
    projections = {shape: _project(shape) for shape in Topology}
    assert {p.shape for p in projections.values()} == set(Topology)


def test_projection_is_frozen() -> None:
    projection = _project(Topology.SQUAD)
    import pytest
    from pydantic import ValidationError

    with pytest.raises((ValidationError, TypeError)):
        projection.shape = Topology.PIPELINE  # type: ignore[misc]
