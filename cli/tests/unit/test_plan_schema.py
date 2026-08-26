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


def test_gate_refuses_every_undatable_created_except_no_frontmatter():
    """Round-6 closed present-but-unreadable; round-9 closed the ABSENT key:
    the intake lanes call this gate on raw frontmatter, and an undatable
    plan there silently minted a bandless node because no fallback dater
    runs on those lanes. Only the EMPTY dict (the no-frontmatter plan,
    fail-soft by design in _read_plan_frontmatter) still passes."""
    from fno.plan.schema import difficulty_gate_error

    err = difficulty_gate_error({"created": "soon"})
    assert err is not None and "cannot read created" in err
    absent = difficulty_gate_error({"claims": "ab-1dea1234"})
    assert absent is not None and "absent from frontmatter" in absent
    assert difficulty_gate_error({"created": "2026-08-27", "difficulty": "high"}) is None
    assert difficulty_gate_error({}) is None


def test_gate_reads_created_exactly_not_truncated():
    """Round-11: the old [:10] slice bound '2026-08-27xyz' to 2026-08-27
    while the model refused the same string - exact ISO forms only, so a
    typo'd date is undatable in both scopes alike."""
    from fno.plan.schema import difficulty_gate_error

    for junk in ("2026-08-27xyz", "2026-08-271"):
        err = difficulty_gate_error({"created": junk, "difficulty": None})
        assert err is not None and "cannot read created" in err, junk
    # Full timestamp and plain date still bind.
    assert difficulty_gate_error({"created": "2026-08-27T04:35"}) is not None
    assert difficulty_gate_error({"created": "2026-08-26"}) is None


def test_model_tier_field_is_gone():
    m = PlanFrontmatter.model_validate(_fm(model_tier="low"))
    assert not hasattr(m, "model_tier")
    assert m.difficulty is None
