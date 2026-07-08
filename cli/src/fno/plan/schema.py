"""fno.plan.schema - one authority for single-doc plan frontmatter.

Replays the proven `fno.config` pattern: a Pydantic model as the single source
of truth for what a plan's YAML frontmatter may contain, plus a drift lint
(`cli/tests/test_plan_schema_drift.py`) that fails CI the moment the model and
the real writers/readers diverge.

Validate-only. This model validates the dict `_doc.load_plan` already parsed
with PyYAML; it never re-serializes a plan. `_stamp.py`'s hand-rolled writer
stays byte-preserving (Locked Decision 1) - a YAML round-trip would reorder
keys and reformat opaque blocks like `kill_criteria`.

`PlanStatus` is derived FROM `_status.STATUS_PROGRESSION` (never re-listed) so
the two definitions cannot drift; `done`/`archived` join as off-axis sibling
terminals, matching `_status`'s own split (Locked Decision 2).
"""
from __future__ import annotations

import enum
from datetime import date, datetime
from typing import Any, Literal

from pydantic import BaseModel, field_validator

from fno.plan._status import STATUS_PROGRESSION, TERMINAL_STATUSES

# Str-enum built directly from the _status axis + terminals. Functional API so
# the members are *derived*, never hand-listed here - the drift lint asserts
# this stays set-equal to _status's own vocabulary.
PlanStatus = enum.Enum(  # type: ignore[misc]
    "PlanStatus",
    {name: name for name in (*STATUS_PROGRESSION, *TERMINAL_STATUSES)},
    type=str,
)


class PlanFrontmatter(BaseModel):
    """The canonical shape of a single-doc plan's YAML frontmatter.

    Required core is the plan==PR==node identity: `node`, `status`, `created`.
    Everything else is optional - a bare plan is valid. `title` is optional on
    purpose: design docs carry the title as the H1, not in frontmatter (every
    plan sampled in internal/fno/{plans,design}/ confirms this).

    Unknown keys are ignored (Pydantic's default) - real plans carry 200+
    distinct historical keys, and this model deliberately does not police them.
    """

    node: str
    status: PlanStatus
    created: date | datetime  # plans carry either a bare date or a full timestamp

    claims: str | None = None  # observed identical to `node` in every sampled plan; not asserted (Open Q1)
    title: str | None = None
    size: Literal["S", "M", "L"] | None = None
    type: str | None = None
    executor: str | None = None
    model_tier: str | None = None
    kind: str | None = None
    parent_epic: str | None = None
    source_doc: str | None = None
    # Scalar OR list-of-mappings - the frontmatter form is predominantly a list
    # (237 list vs 17 scalar in the corpus). The dead `## Kill Criteria`
    # markdown-heading form stays out of scope (Locked Decision 3).
    kill_criteria: str | list[Any] | None = None
    updated: datetime | None = None
    shipped_at: datetime | None = None
    urls: list[str] = []
    session_ids: list[str] = []
    expected_url_count: int | None = None

    @field_validator("status", mode="before")
    @classmethod
    def _coerce_status_bool(cls, v: Any) -> Any:
        """Coerce a YAML-parsed bool back to a string before enum validation.

        Unquoted `status: true` in YAML parses to Python `True`; without this
        the enum would see a bool and reject it with a confusing type error
        instead of naming the (coerced) invalid value. Same bug `_status.py`'s
        coerce_status_from_yaml exists to catch - reproduced here rather than
        reused because that helper rejects `done`/`archived`, which are valid
        for this model's superset enum.
        """
        if isinstance(v, bool):
            return str(v).lower()
        return v
