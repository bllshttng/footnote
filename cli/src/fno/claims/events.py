"""Typed event builders for the seven claim event types.

Mirrors the typed-builder pattern in :mod:`fno.events` so a Python
emit path enforces field shapes at build time, not just at validate().

Audit-trail emission is best-effort: a failure to append to events.jsonl
does not roll back the YAML lock-file write. We log to stderr and return
normally; the lock file is the authoritative record.
"""
from __future__ import annotations

import sys
from typing import Any, Optional

from fno.events import (
    SchemaUnavailableError,
    ValidationError,
    append_event,
    validate,
)

from .types import Claim


CLAIM_SOURCE = "abi-loop"


def _ts_now() -> str:
    import datetime as _dt
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _build(type_name: str, data: dict[str, Any]) -> dict[str, Any]:
    event = {"ts": _ts_now(), "type": type_name, "source": CLAIM_SOURCE, "data": data}
    validate(event)
    return event


def _emit(event: dict[str, Any]) -> None:
    """Append an event to .fno/events.jsonl, best-effort.

    Schema-validate failure or filesystem error logs to stderr and swallows.
    The claim-state-on-disk is the authoritative artifact; the events log
    is for observability.
    """
    try:
        append_event(event)
    except (ValidationError, SchemaUnavailableError, OSError, TimeoutError) as exc:
        print(
            f"claims: failed to emit {event.get('type')!r}: {exc}",
            file=sys.stderr,
        )


def _common(claim: Claim) -> dict[str, Any]:
    """Shared data fields for events that refer to a claim."""
    return {
        "key": claim.key,
        "holder": claim.holder,
        "pid": claim.pid,
        "host": claim.host,
        "acquired_at": claim.acquired_at,
        "expires_at": claim.expires_at,
    }


def emit_claim_acquired(claim: Claim) -> None:
    data = _common(claim)
    if claim.reason is not None:
        data["reason"] = claim.reason
    _emit(_build("claim_acquired", data))


def emit_claim_released(claim: Claim, *, duration_ms: int) -> None:
    data = _common(claim)
    data["duration_held_ms"] = int(duration_ms)
    _emit(_build("claim_released", data))


def emit_claim_refreshed(claim: Claim, *, previous: Claim) -> None:
    data = _common(claim)
    data["previous_expires_at"] = previous.expires_at
    _emit(_build("claim_refreshed", data))


def emit_claim_stale_reclaimed(claim: Claim, *, previous: Claim) -> None:
    data = _common(claim)
    data["previous_holder"] = previous.holder
    data["previous_pid"] = previous.pid
    _emit(_build("claim_stale_reclaimed", data))


def emit_claim_idempotent_reacquired(claim: Claim, *, previous: Claim) -> None:
    data = _common(claim)
    data["previous_acquired_at"] = previous.acquired_at
    _emit(_build("claim_idempotent_reacquired", data))


def emit_claim_force_overridden(
    *,
    key: str,
    reason: str,
    previous_holder: Optional[str],
    previous_pid: Optional[int],
) -> None:
    data: dict[str, Any] = {
        "key": key,
        "override_reason": reason,
    }
    if previous_holder is not None:
        data["previous_holder"] = previous_holder
    if previous_pid is not None:
        data["previous_pid"] = previous_pid
    _emit(_build("claim_force_overridden", data))


# claim_clock_skew_rejected stays registered in the events schema
# (cli/src/fno/events/schema.yaml) for future use. An emit helper
# will land alongside the first concrete clock-skew check inside refresh
# or acquire - PR1 has no path that legitimately rejects on clock skew,
# so adding a helper now would be dead code.


__all__ = [
    "CLAIM_SOURCE",
    "emit_claim_acquired",
    "emit_claim_force_overridden",
    "emit_claim_idempotent_reacquired",
    "emit_claim_refreshed",
    "emit_claim_released",
    "emit_claim_stale_reclaimed",
]
