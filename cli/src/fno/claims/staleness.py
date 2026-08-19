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

Cross-machine claims are treated as opaque: is_live returns False so the
local actor can recover them. The design doc accepts this as a limitation of
the no-shared-state model. "Same machine" is decided by ``hostid``, NOT by
``socket.gethostname()`` directly - see that module for why the hostname is
not a stable identity.
"""
from __future__ import annotations

import time
from typing import Optional

import psutil

from .hostid import is_same_machine
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
      - The claim is on another machine (we cannot remotely verify).
      - The OS does not report claim.pid.
      - The OS-reported create_time for claim.pid is AFTER claim.acquired_at
        (PID reuse: a new process took over the slot).
      - We cannot read the process info (AccessDenied counts as dead).
    """
    if not is_same_machine(claim.host, claim.machine_id):
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
    A TTL claim within its window is LIVE when its pid is live, else SUSPECT
    (x-ba4b) - never reclaimable until the TTL actually expires.

    HYBRID liveness (ab-cc5553f2): a TTL claim whose clock has lapsed is NOT
    unconditionally STALE - if its recorded pid is a live process on this host
    it is still LIVE, because the session is alive (incl. SIGSTOP-suspended)
    even though the TTL expired. This is purely additive: it only ever extends
    liveness, never shortens it. A transient/dead/off-host pid (today's default
    ``os.getpid()`` of the acquire subprocess) fails ``is_live`` -> STALE
    exactly as before, so every non-suspended case is byte-for-byte today.
    ``is_live`` already guards host + pid-reuse (create_time < acquired_at).

    SUSPECT arm (x-ba4b): a TTL claim still inside its window whose recorded pid
    is NOT live reads SUSPECT, not LIVE. Dead-pid-but-unexpired is the respawned-
    worker case; the TTL keeps protecting the slot (acquire/dispatch refuse it
    like LIVE), but the distinct state lets init/dispatch branch. Only TTL expiry
    frees the claim (-> STALE). Mirrors ``claims.rs::classify``.
    """
    if is_expired(claim, now=now):
        # HYBRID: an expired clock does NOT imply a dead session - check the
        # pid before declaring stale (a transient/dead pid still falls to STALE).
        return ClaimState.LIVE if is_live(claim) else ClaimState.STALE
    if claim.expires_at is None:
        return ClaimState.LIVE if is_live(claim) else ClaimState.STALE
    # TTL claim, not yet expired: live pid => LIVE, dead/replaced pid => SUSPECT
    # (TTL-protected, not stealable).
    return ClaimState.LIVE if is_live(claim) else ClaimState.SUSPECT


#: Key families whose holder is a single process that launches something and
#: exits. Nothing respawns under that holder, so SUSPECT protects nothing there.
_ONE_SHOT_KEY_PREFIXES = ("dispatch:",)


def is_one_shot_key(key: str) -> bool:
    """Is KEY held by a process that cannot come back under a new pid?

    SUSPECT (a dead pid inside an unexpired TTL) exists because a `node:` claim
    is held by a SESSION, and a session genuinely can be respawned by the daemon
    under a new pid - x-ba4b measured the disaster when one was treated as free.

    A `dispatch:<id>` reservation is different in kind. Its holder is
    `spawn-cli:<pid>`, one process that launches a worker and exits, so there is
    no respawn to protect and the rationale is structurally inapplicable. The
    generic rule applied it anyway, and a queued spawn that never got a slot
    left a reservation that refused the relaunch with `reason=reservation-held`
    for its whole TTL, recoverable only by hand (x-05be).

    So a dead one-shot holder on this machine is a provable death needing no
    further evidence. A `node:` key is deliberately absent: its liveness needs
    the roster probe, and a shortcut here would archive a live respawned
    worker's claim inside its first stop cycle.
    """
    return key.startswith(_ONE_SHOT_KEY_PREFIXES)


def classify_for_sweep(claim: Claim, now: Optional[int] = None) -> tuple[bool, str]:
    """Classify one claim for GC: can its holder be PROVEN dead from this host?

    The single liveness authority for reaping. Two proofs, and which one a
    claim needs depends on how it declares its own death:

      * An EXPIRED TTL is a wall-clock fact. The holder named the moment its
        lease ends, that moment passed, and reading a clock needs no access to
        the holder's machine. Such a claim is provably dead from anywhere.
      * PID LIVENESS is a local measurement. Proving it needs same-machine
        (by machine_id, never by hostname - see the module header) plus a pid
        that is absent or reused (create_time > acquired_at). Off-machine, that
        proof is unavailable and the claim stays opaque, exactly as
        coordination.md specifies. Weakening this arm would let one machine
        reap another's live claims.

    A dead-pid-but-unexpired TTL claim is SUSPECT, not STALE: the TTL still
    protects the slot for a respawned worker. And on this machine an expired
    TTL whose pid is still live reads LIVE (the hybrid arm in ``classify``),
    so the expiry arm never reaps a running local session.

    Host-independent expiry is what keeps the store from filling forever
    (x-cd1e). ``machine_id`` is authoritative when present, but a claim written
    before that field existed - or by a process that could not read the OS id -
    carries only a hostname, and this machine's hostname MOVES: one box wrote
    ``BB16s-MBP``, ``BB16s-MacBook-Pro.local`` and a tailnet name within an
    hour. Those rows can never satisfy a same-machine proof, so under a
    host-gated sweep they were unreapable for the rest of the disk's life.
    Archiving one changes no liveness answer anywhere: ``classify`` already
    reads an expired claim as not LIVE, so ``list_claims`` already excludes it
    and ``acquire_claim`` already treats the key as recoverable.

    No age threshold: age is a guess, expiry and pid liveness are both
    measurements, and a measurement beats an inference. GC must never reap on
    age alone.

    Returns ``(provably_dead, bucket)`` where ``bucket`` is one of
    ``"offhost"``/``"suspect"``/``"live"`` - only meaningful when
    ``provably_dead`` is False. A single ``classify()`` call backs both the
    bool and the bucket, so a caller scanning many claims (or re-verifying
    one under a mutex) never pays for it twice.
    """
    same_machine = is_same_machine(claim.host, claim.machine_id)
    # The same-machine gate guards the PID arm only. An expired TTL carries its
    # own proof, so it must reach classify() even from a host we cannot verify.
    if not is_expired(claim, now=now) and not same_machine:
        return False, "offhost"
    state = classify(claim, now=now)
    if state is ClaimState.STALE:
        return True, ""
    if state is ClaimState.SUSPECT and same_machine and is_one_shot_key(claim.key):
        # SUSPECT protects a slot whose holder might come back under a new pid.
        # A one-shot holder cannot, so the protection buys nothing and costs a
        # permanent wedge. See is_one_shot_key.
        return True, ""
    return False, "suspect" if state is ClaimState.SUSPECT else "live"


def is_provably_dead(claim: Claim, now: Optional[int] = None) -> bool:
    """Return True iff a claim's holder can be PROVEN dead from this host.

    A thin bool-only view of :func:`classify_for_sweep` for a caller that
    only needs the yes/no answer.
    """
    return classify_for_sweep(claim, now=now)[0]
