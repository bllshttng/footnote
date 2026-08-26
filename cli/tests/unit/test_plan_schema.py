"""AC2-ERR / AC3-EDGE (x-baef): the date-keyed difficulty requirement.

A bare Pydantic-required field would fail 1190 of the 1196 historical plans,
so the gate keys on the plan's own ``created`` date, the same boundary shape
``check_consolidation_file`` uses. Assertions are on the refusal TEXT, not
the exception type: a message that does not name the field and its three
bands teaches nobody anything.
"""
from __future__ import annotations

import pytest
from pydantic import ValidationError

from fno.plan.schema import DIFFICULTY_REQUIRED_AFTER, PlanFrontmatter


def _fm(**over):
    base = {"node": "x-baef", "status": "ready", "created": "2026-08-26", "title": "t"}
    base.update(over)
    return base


def test_post_gate_plan_without_difficulty_refuses_naming_bands():
    for created in ("2026-08-27", "2026-08-27T10:30:00"):
        with pytest.raises(ValidationError) as exc:
            PlanFrontmatter.model_validate(_fm(created=created))
        msg = str(exc.value)
        assert "difficulty" in msg, msg
        assert "low, medium, high" in msg, msg


def test_gate_date_and_older_plans_without_difficulty_pass():
    # On-the-gate-date passes: a plan created ON the gate date predates the
    # gate reaching its author (strictly-after boundary).
    assert PlanFrontmatter.model_validate(_fm(created="2026-08-26")).difficulty is None
    assert PlanFrontmatter.model_validate(_fm(created="2026-01-02")).difficulty is None


def test_post_gate_plan_with_difficulty_passes():
    m = PlanFrontmatter.model_validate(_fm(created="2026-08-27", difficulty="medium"))
    assert m.difficulty == "medium"


def test_gate_constant_is_the_ship_date():
    assert DIFFICULTY_REQUIRED_AFTER.isoformat() == "2026-08-26"


def test_gate_refuses_unreadable_created_but_not_absent():
    """Round-6: a PRESENT but unparseable created must refuse - the
    --execution scope never reaches the model's own date validation, so None
    there was a gate bypass. An ABSENT created stays None: validate-plan.sh's
    filename-date fallback owns that case."""
    from fno.plan.schema import difficulty_gate_error

    err = difficulty_gate_error({"created": "soon"})
    assert err is not None and "cannot read created" in err
    assert difficulty_gate_error({"created": "2026-08-27", "difficulty": "high"}) is None
    assert difficulty_gate_error({}) is None


def test_model_tier_field_is_gone():
    m = PlanFrontmatter.model_validate(_fm(model_tier="low"))
    assert not hasattr(m, "model_tier")
    assert m.difficulty is None
