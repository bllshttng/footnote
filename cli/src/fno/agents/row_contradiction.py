"""Pure rules for refusing self-contradictory agent rows at emission time."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Any, Mapping, Optional, Tuple


_TERMINAL_STATUSES = {"orphaned", "exited"}
_RESUME_BAND_SECONDS = 600

#: (x-d401) How long a stored `spawning` token can stay honest while a live
#: pid exists. A spawn completes when the worker names itself or the row
#: acquires a pid; past this age a still-`spawning` row with a LIVE pid is a
#: token the emitter never refreshed (rows read `spawning` for 3-16 hours
#: while alive, hiding reclaimable roster capacity). Mirrors the spawn gate's
#: own QUEUE_TIMEOUT_S: the gate gives up waiting at the same bound.
SPAWN_TIMEOUT_S = 600.0

#: Supervisor words that are POSITIVE claims of a live, acting process. Only
#: these can contradict a fired falsifier: "idle"/"done" claim nothing, so
#: they stand beside an unreachable verdict without disagreement.
_ACTIVE_SUPERVISOR_WORDS = frozenset({"working", "needs input"})

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
    what made ``liveness_origin: null`` mean five different things, and a reader
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
    """Return the row fields after applying emitter-side contradiction rules.

    ``pid_alive`` is an INPUT the caller measured (census probes it; the raw
    registry row never stores liveness), not a registry field: inject it into
    the mapping before calling. Absent means unmeasured and every rule that
    needs it refuses to fire.
    """
    projected = dict(row)
    # The measurement input never rides the projection out.
    projected.pop("pid_alive", None)
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
    if _spawning_outlived_by_a_live_pid(row, now=now):
        # The movement-derived state with a basis naming the contradiction:
        # a bare `spawning` for a working row is the stored token standing in
        # for a measurement nobody took (x-d401 / x-0248).
        projected["status"] = "live"
        projected["basis"] = "stale-spawning-live-pid"

    # Both keys ALWAYS ride the row, as `reachability`/`basis` and
    # `progress`/`progress_basis` already do on this same row. A conditional
    # key cannot be told apart from a producer that forgot to set one, and the
    # list-row contract in schemas/agents-list-row.json is an exact key set.
    origin, origin_basis = _liveness_origin(row)
    projected["liveness_origin"] = origin
    projected["liveness_origin_basis"] = origin_basis
    # Always rides the row (null = no contradiction): a superseded supervisor
    # claim beside the falsifier that beat it, so the operator reads WHICH
    # source said what rather than only which one won (x-d401 / x-d4a6).
    projected["live_status_basis"] = _supervisor_contradicted(row)
    projected.pop("superseded_live_status", None)
    return projected


def _supervisor_contradicted(row: Mapping[str, Any]) -> Optional[str]:
    """The falsifier that superseded a supervisor's positive claim, or None.

    ``superseded_live_status`` is an INPUT the caller injects (like ``pid``):
    the non-idle supervisor word the truth-status fallback replaced after a
    falsifier fired. The claim it stood against rides ``reachability``/
    ``basis``. The input is popped; only the basis key survives.
    """
    word = row.get("superseded_live_status")
    if not isinstance(word, str) or word.lower() not in _ACTIVE_SUPERVISOR_WORDS:
        return None
    if row.get("reachability") != "unreachable":
        return None
    falsifier = row.get("basis")
    if not isinstance(falsifier, str) or not falsifier:
        return None
    return f"contradicted-by-{falsifier}"


def _spawning_outlived_by_a_live_pid(
    row: Mapping[str, Any], *, now: Any = None
) -> bool:
    """A stored ``spawning`` token a live pid has outlived.

    Fires only on POSITIVE pid liveness (a measured live pid, or a session pid
    the bg rendezvous resolved); unknown liveness keeps the token - a row
    mid-spawn is telling the truth and must not be marked. A missing
    ``created_at`` is absent age evidence, not staleness, so it also keeps
    the token.
    """
    if row.get("status") != "spawning" or row.get("pid_alive") is not True:
        return False
    created_at = _timestamp(row.get("created_at"))
    if created_at is None:
        return False
    now_dt = now or datetime.now(timezone.utc)
    if not isinstance(now_dt, datetime):
        now_dt = _timestamp(now_dt)
    if now_dt is None:
        return False
    return (now_dt - created_at) > timedelta(seconds=SPAWN_TIMEOUT_S)
