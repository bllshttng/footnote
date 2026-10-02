"""Frontmatter status state machine for lean single-doc plan architecture.

Enforces the monotonic progression:
    design -> ready -> in_progress -> in_review

Speaks the same words as the graph `_status` ladder, one per rung. `idea` sits
below `design` as the pre-design rung, and `done`/`superseded` are off-axis
terminals. The graph-to-plan projection lives in the Rust keeper now
(crates/fno-agents/src/plan_doc/status.rs); its two None rows for `blocked`
and `deferred` are the gate that kept a graph-side pause from writing plan
state, and the gate moved with the port.

`idea` was originally absent here, justified by "`idea` has no plan doc so it
never appears" - an assumption `scaffold_separate_plan` invalidated the moment
decompose started writing a doc for a not-yet-designed child. That scaffold
stamped `status: stub`, a word in no vocabulary, so `is_design_stage` read
False for it and the graph derived `ready`: a linked, unfilled scaffold was
fully dispatchable. `idea` is the rung `stub` was always naming.

`shipped` and `archived` were the plan-side spellings of `in_review` and
`superseded`; `stub` was the scaffold spelling of `idea`. All three are retired
as status VALUES and accepted on read via STATUS_ALIASES; `shipped_at` is a
timestamp field and is untouched.

`reviewing`/`shipping` were pruned : they had zero consumers and the
graph has no derived state that distinguishes them, so they never got written.
The reconcile sweep folds them into `in_review` as Tier-1 synonyms.

Backward transitions, identity transitions, and unknown statuses all raise
StatusTransitionError. No silent fallbacks.
"""

from __future__ import annotations

from typing import Any

STATUS_PROGRESSION: tuple[str, ...] = (
    "idea",
    "design",
    "ready",
    "in_progress",
    "in_review",
)

# Off-axis terminals: written directly (graduate stamps `done`; the status
# sweep stamps `superseded`), NOT part of the monotonic axis. Inserting either
# into STATUS_PROGRESSION would break the forward-transition index math.
TERMINAL_STATUSES: tuple[str, ...] = ("done", "superseded")

# Retired spellings, accepted on read and never written. Mirrors the read-and-
# write shape of STATUS_MIGRATION in fno.graph.statuses: a doc stamped under the
# old vocabulary keeps parsing at its correct rung, and nothing rewrites it.
STATUS_ALIASES: dict[str, str] = {
    "shipped": "in_review",
    "archived": "superseded",
    "stub": "idea",
}

# The reconcile sweep and its projection moved to the Rust keeper
# (crates/fno-agents/src/plan_doc/reconcile.rs); the Python copies of the
# projection tables are gone. This module keeps the transition ladder and the
# read-path aliases only.


def _norm_status(raw: object) -> str:
    """Bare lowercase token from a raw frontmatter status value."""
    return str(raw if raw is not None else "").strip().strip("'\"").lower()


def canonical_status(raw: object) -> str:
    """Normalized status with any retired spelling resolved to its survivor.

    Read-path translation only: callers compare and rank against the result,
    they never write it back over the doc that supplied it.
    """
    s = _norm_status(raw)
    return STATUS_ALIASES.get(s, s)


class StatusTransitionError(ValueError):
    """Raised on invalid status transitions."""


def validate_transition(old: str, new: str) -> None:
    """Raise StatusTransitionError on:

    - unknown old or new status
    - backward transition (new index < old index)
    - identity transition (old == new)

    Allow: forward transitions (new index > old index) by any number of steps.

    ``old`` typically comes off a doc, so a retired spelling resolves to its
    survivor before the index math.
    """
    old = STATUS_ALIASES.get(old, old)
    new = STATUS_ALIASES.get(new, new)
    if old not in STATUS_PROGRESSION:
        raise StatusTransitionError(
            f"Unknown status {old!r}. Valid statuses: {list(STATUS_PROGRESSION)}"
        )
    if new not in STATUS_PROGRESSION:
        raise StatusTransitionError(
            f"Unknown status {new!r}. Valid statuses: {list(STATUS_PROGRESSION)}"
        )

    old_index = STATUS_PROGRESSION.index(old)
    new_index = STATUS_PROGRESSION.index(new)

    if new_index == old_index:
        raise StatusTransitionError(
            f"Identity transition rejected: status is already {old!r}. "
            "Provide a different target status."
        )

    if new_index < old_index:
        raise StatusTransitionError(
            f"Backward transition rejected: cannot move from {old!r} (index {old_index}) "
            f"to {new!r} (index {new_index}). "
            f"Status progression is monotonic: {' -> '.join(STATUS_PROGRESSION)}"
        )


def coerce_status_from_yaml(value: Any) -> str:
    """Coerce a raw yaml.safe_load value to a valid status string.

    yaml.safe_load returns Python True for unquoted `status: true` in YAML.
    This function handles that by coercing:
      - bool -> lowercase string ("true" / "false")
      - None -> raises StatusTransitionError
      - anything else -> str(value)

    Then validates the result is in STATUS_PROGRESSION. Raises
    StatusTransitionError if the coerced value is not a known status.
    """
    if value is None:
        raise StatusTransitionError(
            "Status value is None; expected one of: "
            + ", ".join(repr(s) for s in STATUS_PROGRESSION)
        )

    if isinstance(value, bool):
        coerced = str(value).lower()  # True -> "true", False -> "false"
    else:
        coerced = str(value)

    coerced = STATUS_ALIASES.get(coerced, coerced)
    if coerced not in STATUS_PROGRESSION:
        raise StatusTransitionError(
            f"Unknown status {coerced!r} (coerced from {value!r}). "
            f"Valid statuses: {list(STATUS_PROGRESSION)}"
        )

    return coerced
