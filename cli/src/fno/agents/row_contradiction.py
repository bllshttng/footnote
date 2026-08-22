"""Pure rules for refusing self-contradictory agent rows at emission time."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any, Mapping, Optional


_TERMINAL_STATUSES = {"orphaned", "exited"}
_RESUME_BAND_SECONDS = 600


def _timestamp(value: Any) -> Optional[datetime]:
    if value is None:
        return None
    if isinstance(value, (int, float)):
        # macOS stores pid start tokens as epoch microseconds. Linux stores a
        # boot-relative tick count, which cannot be compared to created_at.
        if value > 1_000_000_000_000:
            return datetime.fromtimestamp(value / 1_000_000, timezone.utc)
        return None
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(
            timezone.utc
        )
    except ValueError:
        return None


def project_row(row: Mapping[str, Any], *, now: Any = None) -> dict[str, Any]:
    """Return the row fields after applying emitter-side contradiction rules."""
    projected = dict(row)
    event_at = _timestamp(row.get("last_event_at"))
    reconciled_at = _timestamp(row.get("last_reconciled_at"))
    message_at = _timestamp(row.get("last_message_at"))
    if (
        row.get("status") in _TERMINAL_STATUSES
        and event_at is not None
        and reconciled_at is not None
        and event_at > reconciled_at
    ):
        projected["status"] = "unknown"
        projected["basis"] = "stale-verdict-fresher-event"
    if message_at is not None and event_at is not None and message_at > event_at:
        projected["last_message_at"] = None
        projected["last_message_at_basis"] = "refused-newer-than-transcript"

    has_pid = row.get("pid") is not None
    raw_created_at = row.get("created_at")
    raw_pid_start_time = row.get("pid_start_time")
    created_at = _timestamp(raw_created_at)
    pid_started_at = _timestamp(raw_pid_start_time)
    if not has_pid:
        liveness_origin = None
        liveness_basis = "missing-pid"
    elif raw_created_at is None:
        liveness_origin = None
        liveness_basis = "missing-created-at"
    elif created_at is None:
        liveness_origin = None
        liveness_basis = "unreadable-created-at"
    elif raw_pid_start_time is None:
        liveness_origin = None
        liveness_basis = "missing-pid-start-time"
    elif pid_started_at is None:
        liveness_origin = None
        liveness_basis = "unreadable-pid-start-time"
    elif (pid_started_at - created_at).total_seconds() > _RESUME_BAND_SECONDS:
        liveness_origin = "resumed"
        liveness_basis = None
    else:
        liveness_origin = "survivor"
        liveness_basis = None
    projected["liveness_origin"] = liveness_origin
    projected["liveness_origin_basis"] = liveness_basis
    return projected
