from __future__ import annotations

import pytest
from pydantic import ValidationError

from fno.company.topology import (
    InferenceFacts,
    Topology,
    TopologyRefusal,
    TopologyResolution,
    TopologySource,
    resolve_topology,
)
from fno.roles.models import RoleLayer


def _facts(**overrides: object) -> InferenceFacts:
    base: dict[str, object] = {
        "deliverable_count": 1,
        "has_dependency_edges": False,
        "has_iteration_evaluator": False,
        "has_declared_effect": False,
    }
    base.update(overrides)
    return InferenceFacts.model_validate(base)


def test_ac5_plan_lock_outranks_role_default_and_source_is_plan() -> None:
    result = resolve_topology(
        plan_lock="pipeline",
        role_default="squad",
        role_source_layer=RoleLayer.PROJECT,
        inference_facts=_facts(),
    )
    assert isinstance(result, TopologyResolution)
    assert result.shape is Topology.PIPELINE
    assert result.source is TopologySource.PLAN


def test_ac5_role_default_when_no_plan_lock() -> None:
    result = resolve_topology(
        plan_lock=None,
        role_default="squad",
        role_source_layer=RoleLayer.PROJECT,
        inference_facts=_facts(),
    )
    assert isinstance(result, TopologyResolution)
    assert result.shape is Topology.SQUAD
    assert result.source is TopologySource.ROLE


def test_ac5_inference_when_neither_present() -> None:
    result = resolve_topology(
        plan_lock=None,
        role_default=None,
        role_source_layer=None,
        inference_facts=_facts(deliverable_count=3),
    )
    assert isinstance(result, TopologyResolution)
    assert result.shape is Topology.SQUAD
    assert result.source is TopologySource.INFERENCE


@pytest.mark.parametrize(
    ("facts_overrides", "expected"),
    [
        ({"deliverable_count": 1}, Topology.DIRECT),
        ({"deliverable_count": 1, "has_iteration_evaluator": True}, Topology.LOOP),
        ({"deliverable_count": 3}, Topology.SQUAD),
        ({"deliverable_count": 2, "has_dependency_edges": True}, Topology.PIPELINE),
        # a declared effect does not select a shape (routes to approval boundary)
        ({"deliverable_count": 1, "has_declared_effect": True}, Topology.DIRECT),
        # ordering edges win over count and iteration
        (
            {
                "deliverable_count": 2,
                "has_dependency_edges": True,
                "has_iteration_evaluator": True,
            },
            Topology.PIPELINE,
        ),
        # count wins over iteration for multi-deliverable work
        ({"deliverable_count": 3, "has_iteration_evaluator": True}, Topology.SQUAD),
    ],
)
def test_inference_decision_table_is_total_and_deterministic(
    facts_overrides: dict[str, object], expected: Topology
) -> None:
    result = resolve_topology(
        plan_lock=None,
        role_default=None,
        inference_facts=_facts(**facts_overrides),
    )
    assert isinstance(result, TopologyResolution)
    assert result.shape is expected
    assert result.source is TopologySource.INFERENCE


def test_ac4_invalid_plan_lock_refuses() -> None:
    result = resolve_topology(
        plan_lock="star",
        role_default="squad",
        role_source_layer=RoleLayer.PLAN,
        inference_facts=_facts(),
    )
    assert isinstance(result, TopologyRefusal)
    assert result.value == "star"


def test_ac4_invalid_role_default_refuses_naming_source_layer() -> None:
    result = resolve_topology(
        plan_lock=None,
        role_default="star",
        role_source_layer=RoleLayer.PLAN,
        inference_facts=_facts(),
    )
    assert isinstance(result, TopologyRefusal)
    assert result.value == "star"
    assert result.source_layer is RoleLayer.PLAN


def test_ac3_resolution_carries_only_shape_and_source() -> None:
    # Frozen with extra=forbid: identity, authority, and required evidence
    # structurally cannot ride along on a topology resolution.
    resolution = TopologyResolution.model_validate(
        {"shape": "direct", "source": "inference"}
    )
    assert set(TopologyResolution.model_fields) == {"shape", "source"}
    with pytest.raises(ValidationError):
        TopologyResolution.model_validate(
            {"shape": "direct", "source": "inference", "work_order": "x-1"}
        )


def test_inference_facts_rejects_negative_count() -> None:
    with pytest.raises(ValidationError):
        InferenceFacts.model_validate({"deliverable_count": -1})
