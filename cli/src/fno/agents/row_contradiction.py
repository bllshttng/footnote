"""Pure rules for refusing self-contradictory agent rows at emission time."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any, Mapping, Optional, Tuple


_TERMINAL_STATUSES = {"orphaned", "exited"}
_RESUME_BAND_SECONDS = 600

# A start token at or below this reads as clock ticks since boot, not epoch
# microseconds, so it cannot be compared to created_at. Both languages share
# the bound; see `row_timestamp` in crates/fno-agents/src/daemon.rs.
_EPOCH_MICROS_FLOOR = 1_000_000_000_000


def _timestamp(value: Any) -> Optional[datetime]:
    if value is None:
        return None
    if isinstance(value, (int, float)):
        if value > _EPOCH_MICROS_FLOOR:
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


def _read_field(
    row: Mapping[str, Any], key: str, label: str
) -> Tuple[Optional[datetime], Optional[str]]:
    """Parse one timestamp field into ``(value, basis)``.

    Absent and unreadable are separated deliberately. Folding them together is
    what made ``liveness_origin: null`` mean four different things, and a reader
    holding one null could not tell "nothing was recorded" from "something was
    recorded that this parser cannot read".
    """
    raw = row.get(key)
    if raw is None:
        return None, f"{label}-absent"
    parsed = _timestamp(raw)
    if parsed is None:
        return None, f"{label}-unreadable"
    return parsed, None


def _liveness_origin(
    row: Mapping[str, Any],
) -> Tuple[Optional[str], Optional[str]]:
    """Return ``(liveness_origin, basis)`` for one row.

    THE PID GATE COMES FIRST, and it is the whole reason this lives in one
    function. The Rust producer checked it and the Python one did not, so a
    pidless row read `survivor` on one path and null on the other: one field,
    two reachable implementations, one guard. The row's origin describes the
    process the row names, so with no pid there is no process to describe.

    A non-null origin carries no basis. The value IS the evidence there, and
    the pair (value, no basis) has one reading. A null always carries one.
    """
    if row.get("pid") is None:
        return None, "pid-absent"
    created_at, basis = _read_field(row, "created_at", "created-at")
    if created_at is None:
        return None, basis
    pid_started_at, basis = _read_field(row, "pid_start_time", "pid-start")
    if pid_started_at is None:
        return None, basis
    if (pid_started_at - created_at).total_seconds() > _RESUME_BAND_SECONDS:
        return "resumed", None
    return "survivor", None


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

    origin, origin_basis = _liveness_origin(row)
    projected["liveness_origin"] = origin
    if origin_basis is not None:
        projected["liveness_origin_basis"] = origin_basis
    return projected
