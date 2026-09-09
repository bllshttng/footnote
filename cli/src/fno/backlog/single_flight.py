"""One in flight per backlog arm scope (x-ef2c).

Measured 2026-09-09 at load 458 against a gate of 120: three concurrent
`backlog advance --epic` from three different parents, and three concurrent
`backlog reconcile` trees, the oldest twelve minutes. Both commands are slow
while the graph is contended, their arms fire on a fixed interval, and when a
run outlives the interval the next fire stacks on top of it. Each copy
contends for the same store, every run gets slower, more copies stack: the
load feeds itself.

The remedy is a latch, not a faster command. daemon.rs already applies it to
its retirement sweep behind `gc_in_flight`; crates/fno-agents has the
cross-process form in `single_flight.rs`. This is that latch for the two
Python arms, living inside the commands themselves so every parent is
covered - daemon drains, merge paths, groom legs, SessionStart hooks, and any
orphan whose parent died mid-run.

A second invocation for an already-in-flight scope is HELD, not run: it
reports `held` with a request count and exits 0. A skipped tick is correct
behavior when the previous tick is still running - the work is not lost, it
is in progress. The instrument is a claim (atomic lockfile, pid probe, TTL),
the same store `fno agents claim status` reads, so an operator can see who
holds a scope. A holder whose process dies is reclaimed on the pid probe, not
the TTL.
"""

from __future__ import annotations

import json
import os
import time
import uuid
from dataclasses import dataclass
from typing import Any, Optional

import typer

from fno.claims import (
    ClaimHeldByOther,
    acquire_claim,
    claim_status,
    force_release_claim,
    release_claim,
)
from fno.claims.io import (
    claim_path,
    claims_dir,
    claims_root_for,
    encode_key,
    read_claim_file,
)

# How long a flight's claim outlives its holder without a release. Measured
# reconcile runs reach twelve minutes under contention; half an hour covers
# that with margin, and a run that somehow outlives it degrades to today's
# behavior (a second copy may start) rather than wedging the scope shut.
FLIGHT_TTL_MS = 30 * 60 * 1000

_ADVANCE_PREFIX = "flight:backlog-advance"
_RECONCILE_PREFIX = "flight:backlog-reconcile"


def advance_flight_key(epic: Optional[str]) -> str:
    """The gate scope for one `fno backlog advance` invocation.

    An epic converge and a board advance are different work over the same
    store, so each holds its own gate; two copies of the SAME scope are the
    stacking this exists to delete.
    """
    if epic:
        return f"{_ADVANCE_PREFIX}:epic:{epic}"
    return _ADVANCE_PREFIX


def reconcile_flight_key(*, node: Optional[str], pr_number: Optional[int]) -> str:
    """The gate scope for one `fno backlog reconcile` invocation.

    A full sweep owns the graph; a `--pr-number` / `--node` pass owns one
    PR's or one node's closure. Distinct scopes, distinct gates, so a merge's
    own closure never queues behind an unrelated twelve-minute sweep.
    """
    if node:
        return f"{_RECONCILE_PREFIX}:node:{node}"
    if pr_number is not None:
        return f"{_RECONCILE_PREFIX}:pr:{pr_number}"
    return _RECONCILE_PREFIX


@dataclass
class FlightGate:
    """This invocation holds the scope. `release()` after the work."""

    key: str
    holder: str

    def release(self) -> None:
        try:
            release_claim(self.key, self.holder, root=claims_root_for(self.key))
        except Exception:
            # The TTL plus the pid probe retire an unreleased claim; a failed
            # release must never mask the work's own outcome.
            pass


@dataclass
class FlightHeld:
    """The scope was already in flight; this invocation ran nothing."""

    key: str
    holder: str
    held_for_s: int
    requests: int

    def payload(self) -> dict[str, Any]:
        return {
            "held": True,
            "requests": self.requests,
            "holder": self.holder,
            "held_for_s": self.held_for_s,
        }


def acquire_flight(key: str, *, scope: str) -> FlightGate | FlightHeld:
    """Take the one-in-flight gate for `key`, or report it held.

    The holder string is unique per invocation: an identical holder would
    read as an idempotent re-acquire and let a second copy straight through.
    """
    reason = f"backlog single-flight: {scope}"
    root = claims_root_for(key)
    holder = f"single-flight:{os.getpid()}:{uuid.uuid4().hex[:8]}"
    try:
        acquire_claim(key, holder, reason=reason, ttl_ms=FLIGHT_TTL_MS, root=root)
        return FlightGate(key=key, holder=holder)
    except ClaimHeldByOther:
        pass
    if _holder_process_is_dead(key):
        # Reclaim and take the scope. A new acquirer may win the dropped
        # claim first; then this invocation reports held against it.
        force_release_claim(key, "single-flight holder process is gone", root=root)
        try:
            acquire_claim(key, holder, reason=reason, ttl_ms=FLIGHT_TTL_MS, root=root)
            return FlightGate(key=key, holder=holder)
        except ClaimHeldByOther:
            pass
    status = claim_status(key, root=root)
    acquired_at = status.get("acquired_at") or 0
    return FlightHeld(
        key=key,
        holder=str(status.get("holder") or "unknown"),
        held_for_s=max(0, int(time.time() * 1000 - acquired_at) // 1000),
        requests=_count_held_request(key),
    )


def _holder_process_is_dead(key: str) -> bool:
    """Probe the holder's pid directly.

    The claims layer reads a holder as alive while its creating session's
    transcript is live (`transcript-live`). That is the right policy for a
    node claim a respawned worker re-anchors, and the wrong one for a gate on
    a subprocess: the holder pid IS the holder here, and a killed run must
    not hold the scope shut for the TTL. An unreadable claim probes as alive;
    the TTL retires it.
    """
    try:
        claim = read_claim_file(claim_path(key, root=claims_root_for(key)))
    except Exception:
        return False
    pid = claim.pid
    if not pid:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    except OSError:
        return False
    return False


def report_held(held: FlightHeld, verb: str, *, json_out: bool) -> None:
    """Print the held receipt. Exit stays 0: a held tick is not an error."""
    if json_out:
        typer.echo(json.dumps(held.payload()))
        return
    typer.echo(
        f"{verb}: held, a run for this scope is already in flight "
        f"(holder={held.holder}, held_for_s={held.held_for_s}s, "
        f"requests={held.requests}); the in-flight run owns the work, "
        "this one stood down"
    )


def _count_held_request(key: str) -> int:
    """Record one held request and return the scope's running count.

    The count is the positive marker that the gate engages - the cross-process
    counterpart of the reap arm's `requests=N`. One appended line per request;
    the read-back can over-count by a concurrent appender, which a report-only
    number can afford. A lost write returns 0 and the receipt still says held.
    """
    try:
        path = claims_dir(claims_root_for(key)) / f"{encode_key(key)}.held-requests"
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(f"{int(time.time() * 1000)} {os.getpid()}\n")
        with open(path, encoding="utf-8") as fh:
            return sum(1 for line in fh if line.strip())
    except OSError:
        return 0
