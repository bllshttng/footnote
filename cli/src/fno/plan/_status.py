"""Frontmatter status state machine for lean single-doc plan architecture.

Enforces the monotonic progression:
    design -> ready -> in_progress -> reviewing -> shipping -> shipped

Backward transitions, identity transitions, and unknown statuses all raise
StatusTransitionError. No silent fallbacks.
"""

from __future__ import annotations

from typing import Any

STATUS_PROGRESSION: tuple[str, ...] = (
    "design",
    "ready",
    "in_progress",
    "reviewing",
    "shipping",
    "shipped",
)


class StatusTransitionError(ValueError):
    """Raised on invalid status transitions."""


def validate_transition(old: str, new: str) -> None:
    """Raise StatusTransitionError on:

    - unknown old or new status
    - backward transition (new index < old index)
    - identity transition (old == new)

    Allow: forward transitions (new index > old index) by any number of steps.
    """
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
        coerced = str(value)

    if coerced not in STATUS_PROGRESSION:
        raise StatusTransitionError(
            f"Unknown status {coerced!r} (coerced from {value!r}). "
            f"Valid statuses: {list(STATUS_PROGRESSION)}"
        )

    return coerced
