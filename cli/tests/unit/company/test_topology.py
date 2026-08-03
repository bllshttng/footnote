from __future__ import annotations

import pytest
from pydantic import ValidationError

from fno.company.topology import (
    Topology,
    TopologyRefusal,
    validate_manifest_topology,
)
from fno.roles.models import RoleLayer


def test_four_members_are_complete_and_closed() -> None:
    assert {m.value for m in Topology} == {"direct", "loop", "squad", "pipeline"}


def test_each_legal_literal_resolves() -> None:
    for literal in ("direct", "loop", "squad", "pipeline"):
        resolved = validate_manifest_topology(literal)
        assert isinstance(resolved, Topology)
        assert resolved.value == literal


def test_ac4_err_unknown_literal_refuses_with_value_and_source_layer() -> None:
    refusal = validate_manifest_topology("star", source_layer=RoleLayer.PLAN)
    assert isinstance(refusal, TopologyRefusal)
    assert refusal.value == "star"
    assert refusal.source_layer is RoleLayer.PLAN
    assert "direct" in refusal.recovery


def test_refusal_source_layer_defaults_none_for_bare_validation() -> None:
    refusal = validate_manifest_topology("star")
    assert isinstance(refusal, TopologyRefusal)
    assert refusal.source_layer is None


def test_validator_never_raises_on_arbitrary_input() -> None:
    # An offending-value field must accept the offending value verbatim.
    for bad in ("", "   ", "star", "DIRECT", "Direct", "multi word value"):
        result = validate_manifest_topology(bad)
        assert isinstance(result, TopologyRefusal)
        assert result.value == bad


def test_refusal_is_frozen_and_rejects_extra_fields() -> None:
    refusal = validate_manifest_topology("star")
    assert isinstance(refusal, TopologyRefusal)
    with pytest.raises((ValidationError, TypeError)):
        refusal.value = "other"  # type: ignore[misc]
    with pytest.raises(ValidationError):
        TopologyRefusal.model_validate(
            {"value": "star", "source_layer": None, "recovery": "x", "extra": 1}
        )


def test_refusal_requires_a_recovery_step() -> None:
    with pytest.raises(ValidationError):
        TopologyRefusal.model_validate({"value": "star", "source_layer": None})
