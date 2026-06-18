"""Liveness checks for a claim's holder process.

Two questions:

    is_live(claim) -> bool
        For PID-liveness claims (no expires_at), is the holder process
        still running on this host with the right create_time?

    is_expired(claim, now_ms=None) -> bool
        For TTL claims, has expires_at passed?

PID-reuse detection compares ``acquired_at`` against
``psutil.Process(pid).create_time() * 1000``. If the OS-reported create
time is *after* acquired_at, the PID has been reused by a different
process since the claim was filed.

Cross-host claims (claim.host != socket.gethostname()) are treated as
opaque: is_live returns False so the local actor can recover them. The
design doc accepts this as a limitation of the no-shared-state model.
"""
from __future__ import annotations

import socket
import time
from typing import Optional

import psutil

from .types import Claim, ClaimState


def now_ms() -> int:
    """Return current UTC time as epoch milliseconds."""
    return int(time.time() * 1000)


def _process_create_time_ms(pid: int) -> Optional[int]:
    """Return the holder's process create time in epoch-ms, or None if absent.

    None means "the OS does not report this PID" - treat as dead.
    Permission errors are also treated as None: a holder we cannot inspect
    is one we cannot validate, so we cannot prove it's still ours.
    """
    try:
        proc = psutil.Process(pid)
        return int(proc.create_time() * 1000)
    except (psutil.NoSuchProcess, psutil.AccessDenied):
        return None


def is_live(claim: Claim) -> bool:
    """Return True iff the claim's holder is verifiably running.

    Returns False if:
      - The claim is on another host (we cannot remotely verify).
      - The OS does not report claim.pid.
      - The OS-reported create_time for claim.pid is AFTER claim.acquired_at
        (PID reuse: a new process took over the slot).
      - We cannot read the process info (AccessDenied counts as dead).
    """
    if claim.host != socket.gethostname():
        return False

    create_ms = _process_create_time_ms(claim.pid)
    if create_ms is None:
        return False

    # PID-reuse: the process currently holding this PID started AFTER the
    # claim was filed, so it is a different process. The original holder
    # died and the kernel handed the slot to someone else.
    if create_ms > claim.acquired_at:
        return False

    return True


def is_expired(claim: Claim, now: Optional[int] = None) -> bool:
    """Return True iff a TTL claim has passed its expires_at.

    PID-liveness claims (expires_at is None) are NEVER "expired" - their
    staleness is determined by is_live() instead. Returning False here for
    PID-liveness keeps the two axes separate.
    """
    if claim.expires_at is None:
        return False
    if now is None:
        now = now_ms()
    return now >= claim.expires_at


def classify(claim: Claim, now: Optional[int] = None) -> ClaimState:
    """Compose is_live + is_expired into a state classification.

    A PID-liveness claim is STALE when the holder process is dead or replaced.
    A TTL claim within its window is LIVE regardless of PID.

    HYBRID liveness (ab-cc5553f2): a TTL claim whose clock has lapsed is NOT
    unconditionally STALE - if its recorded pid is a live process on this host
    it is still LIVE, because the session is alive (incl. SIGSTOP-suspended)
    even though the TTL expired. This is purely additive: it only ever extends
    liveness, never shortens it. A transient/dead/off-host pid (today's default
    ``os.getpid()`` of the acquire subprocess) fails ``is_live`` -> STALE
    exactly as before, so every non-suspended case is byte-for-byte today.
    ``is_live`` already guards host + pid-reuse (create_time < acquired_at).
    """
    if is_expired(claim, now=now):
        # HYBRID: an expired clock does NOT imply a dead session - check the
        # pid before declaring stale (a transient/dead pid still falls to STALE).
        return ClaimState.LIVE if is_live(claim) else ClaimState.STALE
    if claim.expires_at is None:
        return ClaimState.LIVE if is_live(claim) else ClaimState.STALE
    # TTL claim, not yet expired - live regardless of PID
    return ClaimState.LIVE
