"""Anti-drift guards for the plan-frontmatter schema.

Mirrors ``test_config_schema_drift.py``: ``fno.plan.schema.PlanFrontmatter`` is
the single source of truth for plan frontmatter, and these tests fail CI the
moment the model drifts from its two upstream sources - ``_status.py`` (the
status vocabulary the enum is derived from) and ``_stamp.py`` (the ship-time
writer whose keys the model must cover).

This is the one git-CI piece of the plan-schema feature (Locked Decision 4):
plans themselves live in the untracked vault, so the model - which lives in the
repo - is what CI can protect.
"""
from __future__ import annotations

import re
from pathlib import Path

import fno.plan._stamp as _stamp_mod
from fno.plan._status import STATUS_PROGRESSION, TERMINAL_STATUSES
from fno.plan.schema import PlanFrontmatter, PlanStatus


def test_plan_status_axis_matches_status_module() -> None:
    """PlanStatus members == STATUS_PROGRESSION plus exactly {done, archived} (AC2-HP).

    Fails the build the moment someone edits the axis in ``_status.py`` (or the
    enum here) without the other - the exact drift-killer the config schema
    already uses.
    """
    members = {m.value for m in PlanStatus}
    axis = set(STATUS_PROGRESSION)
    terminals = set(TERMINAL_STATUSES)

    assert terminals == {"done", "archived"}, (
        f"off-axis terminals drifted: {sorted(terminals)} != ['archived', 'done']"
    )
    assert members == axis | terminals, (
        "PlanStatus drifted from _status.py; the enum must be derived from "
        "STATUS_PROGRESSION + TERMINAL_STATUSES (never hand-listed)"
    )
    # done/archived are off the monotonic axis, never inserted into it.
    assert members - axis == terminals


def test_stamp_written_fields_are_modeled() -> None:
    """Every frontmatter key ``_stamp.py`` writes has a PlanFrontmatter field.

    Catches the drift class where the ship-time writer starts emitting a key
    the schema doesn't know about, so ``fno plan validate`` would silently pass
    a plan carrying an unmodeled ship field.
    """
    src = Path(_stamp_mod.__file__).read_text(encoding="utf-8")
    written = set(re.findall(r'fields\["(\w+)"\]\s*=', src))

    # Guard against the regex going inert (a refactor renaming `fields`): the
    # load-bearing ship keys must always be found.
    documented = {"status", "shipped_at", "urls", "session_ids", "expected_url_count"}
    assert documented <= written, (
        f"stamp-writer regex missed documented keys: {sorted(documented - written)}"
    )

    modeled = set(PlanFrontmatter.model_fields)
    missing = written - modeled
    assert not missing, (
        f"_stamp.py writes frontmatter keys with no PlanFrontmatter field: {sorted(missing)}"
    )
