"""Frontmatter status state machine for lean single-doc plan architecture.

Enforces the monotonic progression:
    design -> ready -> in_progress -> in_review

This is the SAME ladder the graph `_status` speaks (x-5d91), not a parallel
dialect: idea -> design -> ready -> in_progress -> in_review -> done, with
blocked/deferred/superseded orthogonal. `idea` has no plan doc so it never
appears here, and `done`/`archived` are off-axis terminals.

`reviewing`/`shipping` were pruned (x-f34f); `shipped` was renamed to
`in_review` (x-5d91). All are folded in as read-time synonyms, so vault docs
carrying the old vocabulary keep working untouched.

Backward transitions, identity transitions, and unknown statuses all raise
StatusTransitionError. No silent fallbacks.
"""

from __future__ import annotations

from typing import Any, Optional

STATUS_PROGRESSION: tuple[str, ...] = (
    "design",
    "ready",
    "in_progress",
    "in_review",
)

# Legacy plan-frontmatter statuses -> current vocabulary. `shipped` was renamed
# to `in_review` so the plan axis and the graph `_status` vocabulary are the one
# lifecycle ladder (design -> ready -> in_progress -> in_review -> done) rather
# than two dialects needing translation. Vault docs stamped `shipped` are read
# as `in_review`; nothing needs rewriting on disk.
PLAN_STATUS_SYNONYMS: dict[str, str] = {"shipped": "in_review"}

# Off-axis terminals: written directly (graduate stamps `done`; the status
# sweep stamps `archived`), NOT part of the monotonic axis. Inserting either
# into STATUS_PROGRESSION would break the forward-transition index math.
TERMINAL_STATUSES: tuple[str, ...] = ("done", "archived")

# The full canonical plan-status vocabulary: axis + terminals. The reconcile
# sweep leaves any status in this set untouched (it corrects drift only).
KNOWN_STATUSES: frozenset[str] = frozenset(STATUS_PROGRESSION) | frozenset(TERMINAL_STATUSES)

# Graph derived `_status` -> plan `status` projection (x-f34f). Total over the
# graph vocabulary; None means "no plan write" (a graph-side gate that must not
# touch plan state). Since x-5d91 the two vocabularies are the same ladder, so
# this is near-identity - it survives to carry the genuinely non-identity rows
# (idea -> design, superseded -> archived, and the two None gates).
GRAPH_TO_PLAN_STATUS: dict[str, str | None] = {
    "idea": "design",  # node exists, no plan doc yet
    "design": "design",  # doc exists but is still a design doc
    "ready": "ready",
    "in_progress": "in_progress",
    "claimed": "in_progress",  # legacy graph vocabulary, pre-x-5d91 rows
    "blocked": None,  # graph-side gate; plan keeps its current state
    "in_review": "in_review",  # PR open = implementation complete
    "done": "done",  # merged
    "superseded": "archived",
    "deferred": None,  # pause is reversible; plan state stands
}

# Forward-only ordering for the projection. `done` caps the axis; `archived`
# (from `superseded`) is a terminal reachable from any non-terminal state and is
# never rank-compared.
_PROJECTION_RANK: dict[str, int] = {
    "design": 0,
    "ready": 1,
    "in_progress": 2,
    "in_review": 3,
    "done": 4,
}


def _norm_status(raw: object) -> str:
    """Bare lowercase token from a raw frontmatter status value.

    Folds legacy synonyms (see PLAN_STATUS_SYNONYMS) so a vault doc stamped
    with the old vocabulary compares equal to its current name.
    """
    token = str(raw if raw is not None else "").strip().strip("'\"").lower()
    return PLAN_STATUS_SYNONYMS.get(token, token)


def project_plan_status(current: object, graph_status: str) -> Optional[str]:
    """Plan status to WRITE for a node in ``graph_status``, or None to leave it.

    Forward-only along design < ready < in_progress < in_review < done. Returns
    None when the graph status maps to no write, the target equals the current
    status, or the target would be a backward move (graph wins forward, a human
    hand-edit wins backward). ``archived`` is written over any non-terminal
    plan state (superseded) but never over ``done`` or ``archived``.
    """
    target = GRAPH_TO_PLAN_STATUS.get(graph_status)
    if not target:
        return None
    cur = _norm_status(current)
    if target == cur:
        return None
    if target == "archived":
        return None if cur in ("done", "archived") else "archived"
    # target is a forward-axis status (design..done)
    if cur in ("done", "archived"):
        return None  # terminal: never auto-rewritten forward off a terminal
    if _PROJECTION_RANK[target] <= _PROJECTION_RANK.get(cur, -1):
        return None
    return target


class StatusTransitionError(ValueError):
    """Raised on invalid status transitions."""


def validate_transition(old: str, new: str) -> None:
    """Raise StatusTransitionError on:

    - unknown old or new status
    - backward transition (new index < old index)
    - identity transition (old == new)

    Allow: forward transitions (new index > old index) by any number of steps.
    """
    old = _norm_status(old)
    new = _norm_status(new)
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

    Per feedback_literal_string_rejects_yaml_bool memory entry.
    """
    if value is None:
        raise StatusTransitionError(
            "Status value is None; expected one of: "
            + ", ".join(repr(s) for s in STATUS_PROGRESSION)
        )

    if isinstance(value, bool):
        coerced = str(value).lower()  # True -> "true", False -> "false"
    else:
        coerced = _norm_status(value)

    if coerced not in STATUS_PROGRESSION:
        raise StatusTransitionError(
            f"Unknown status {coerced!r} (coerced from {value!r}). "
            f"Valid statuses: {list(STATUS_PROGRESSION)}"
        )

    return coerced
