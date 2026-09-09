"""One in flight per backlog arm scope.

Measured 2026-09-09 at load 458 against a gate of 120: three concurrent
`backlog advance --epic` from three different parents and three concurrent
`backlog reconcile` trees, the oldest twelve minutes. A run slower than the
arms' fixed interval stacked the next fire on top of itself, and each copy
made the store slower for the rest. The latch lives inside the commands, so
every parent is covered: daemon drains, merge paths, groom legs, SessionStart
hooks, orphans. A second invocation for an in-flight scope is HELD, not run:
it reports `held` with a request count and exits 0 - a skipped tick is
correct behavior when the previous tick is still running. The instrument is
a claim, the same store `fno agents claim status` reads.
"""

from __future__ import annotations

import contextlib
import json
import os
import time
import uuid
from dataclasses import dataclass
from typing import Any, Callable, Iterator, Optional

import typer

from fno.claims import ClaimHeldByOther, acquire_claim, claim_status, force_release_claim, release_claim
from fno.claims.io import claim_path, claims_dir, claims_root_for, encode_key, read_claim_file

# Twelve-minute reconcile runs are measured; 30 minutes bounds a lost holder,
# and a run that outlives it degrades to the pre-gate behavior.
FLIGHT_TTL_MS = 30 * 60 * 1000

_ADVANCE_PREFIX = "flight:backlog-advance"
_RECONCILE_PREFIX = "flight:backlog-reconcile"


def advance_flight_key(epic: Optional[str]) -> str:
    """An epic converge and a board advance are different work; same-scope
    copies are the stacking this deletes."""
    return f"{_ADVANCE_PREFIX}:epic:{epic}" if epic else _ADVANCE_PREFIX


def reconcile_flight_key(*, node: Optional[str], pr_number: Optional[int]) -> str:
    """A full sweep owns the graph; a `--pr-number`/`--node` pass owns one
    PR's closure and never queues behind an unrelated sweep."""
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
            # The TTL plus the pid probe retire it; never mask the work's outcome.
            pass


@dataclass
class FlightHeld:
    """The scope was already in flight; this invocation ran nothing."""

    key: str
    holder: str
    held_for_s: int
    requests: int

    def payload(self) -> dict[str, Any]:
        return {"held": True, "requests": self.requests, "holder": self.holder, "held_for_s": self.held_for_s}


def acquire_flight(key: str, *, scope: str) -> FlightGate | FlightHeld:
    """Take the one-in-flight gate, or report it held. The holder string is
    unique per invocation: an identical holder reads as an idempotent
    re-acquire and would let a second copy straight through."""
    reason = f"backlog single-flight: {scope}"
    root = claims_root_for(key)
    holder = f"single-flight:{os.getpid()}:{uuid.uuid4().hex[:8]}"
    try:
        acquire_claim(key, holder, reason=reason, ttl_ms=FLIGHT_TTL_MS, root=root)
        return FlightGate(key=key, holder=holder)
    except ClaimHeldByOther:
        pass
    if _holder_process_is_dead(key):
        # A killed run must not hold the scope shut for the TTL; a new
        # acquirer may win the dropped claim, and then this one reports held.
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


def acquire_flight_open(key: str, *, scope: str) -> FlightGate | FlightHeld | None:
    """Fail open: a gate that cannot run (sandboxed state root, contention
    exhausted) returns None and the caller proceeds ungated. The verbs this
    guards promise "always exits 0"; the protection must never traceback."""
    try:
        return acquire_flight(key, scope=scope)
    except Exception as exc:  # noqa: BLE001 - fail open, never break the verb
        typer.echo(f"warning: single-flight gate unavailable for {key} ({exc}); proceeding ungated", err=True)
        return None


def _holder_process_is_dead(key: str) -> bool:
    """Probe the holder pid directly. The claims layer reads a holder live
    through its session transcript - right for a node claim, wrong for a gate
    on a subprocess: here the holder pid IS the holder. Unreadable probes
    alive; the TTL retires it."""
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


def _count_held_request(key: str) -> int:
    """Append one held request and return the scope's running count - the
    cross-process counterpart of the reap arm's `requests=N`. The read-back
    can over-count by one; report-only. A lost write returns 0."""
    try:
        path = claims_dir(claims_root_for(key)) / f"{encode_key(key)}.held-requests"
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(f"{int(time.time() * 1000)} {os.getpid()}\n")
        with open(path, encoding="utf-8") as fh:
            return sum(1 for line in fh if line.strip())
    except OSError:
        return 0


def report_held(held: FlightHeld, verb: str, *, json_out: bool, extra: Optional[dict] = None) -> None:
    """Print the held receipt. Exit stays 0: a held tick is not an error."""
    if json_out:
        payload = held.payload()
        if extra:
            payload.update(extra)
        typer.echo(json.dumps(payload))
        return
    typer.echo(
        f"{verb}: held, a run for this scope is already in flight "
        f"(holder={held.holder}, held_for_s={held.held_for_s}s, requests={held.requests}); "
        "the in-flight run owns the work, this one stood down"
    )


@contextlib.contextmanager
def advance_flight_scope(epic: Optional[str], *, json_out: bool) -> Iterator[bool]:
    """Gate one advance invocation. Yields False (already reported held) when
    the scope is in flight; the caller returns without doing work. `--stop`
    never enters here: deactivating a mission is a control action, not a
    converge, and never queues behind its own drain."""
    with _flight_scope(advance_flight_key(epic), f"advance --epic {epic}" if epic else "advance",
                       "backlog advance", json_out,
                       extra=None if epic else {"decision": "held"}) as ok:
        yield ok


def reconcile_gate(
    *,
    dry_run: bool,
    node: Optional[str],
    json_out: bool,
    pr_number: Optional[int],
    once: Callable[[], None],
) -> None:
    """`cmd_reconcile`'s entry: the mutual-exclusion refusal, the dry-run
    bypass (--dry-run mutates nothing and stays readable mid-sweep), then the
    one-in-flight gate around one reconcile pass."""
    if node is not None and pr_number is not None:
        raise typer.BadParameter(
            "--node and --pr-number are mutually exclusive: --pr-number "
            "already scopes the scan to every node its own trailer claims, "
            "which --node cannot narrow without silently stranding the "
            "other claimed nodes stamped-but-unclosed. Run them separately."
        )
    if dry_run:
        once()
        return
    with _flight_scope(
        reconcile_flight_key(node=node, pr_number=pr_number), "reconcile", "backlog reconcile", json_out
    ) as ok:
        if ok:
            once()


@contextlib.contextmanager
def _flight_scope(
    key: str, scope: str, verb: str, json_out: bool, extra: Optional[dict] = None
) -> Iterator[bool]:
    flight = acquire_flight_open(key, scope=scope)
    try:
        if isinstance(flight, FlightHeld):
            report_held(flight, verb, json_out=json_out, extra=extra)
            yield False
        else:
            yield True
    finally:
        if isinstance(flight, FlightGate):
            flight.release()
