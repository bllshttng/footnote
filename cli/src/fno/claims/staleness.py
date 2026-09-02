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


# The liveness cause vocabulary: HOW a holder passed or failed the pid check.
# A verdict beside its cause is the house shape (classify_for_sweep already
# returns one); without it, "suspect" is five different situations wearing
# one word. Mirrored by the Rust leg - the parity harness pins the strings.
CAUSE_LIVE = "live"
CAUSE_OFFHOST = "offhost"
CAUSE_PID_UNAVAILABLE = "pid-unavailable"
CAUSE_PID_ABSENT = "pid-absent"
CAUSE_ACCESS_DENIED = "access-denied"
CAUSE_PID_REUSE = "pid-reuse"


def _probe_create_time(pid: Optional[int]) -> tuple[Optional[int], str]:
    """Ask the OS for the holder's create time, and name the failure if none.

    Returns ``(create_ms, cause)``; an empty cause means the read succeeded.

    The split is load-bearing. NoSuchProcess means the pid has no holder -
    provably dead. AccessDenied means the holder EXISTS and this process may
    not inspect it, because permission cannot be denied on a pid that is
    gone. Collapsing them into one None made a live-but-unreadable holder
    read as dead, and its claim stealable.
    """
    if pid is None:
        return None, CAUSE_PID_UNAVAILABLE
    try:
        proc = psutil.Process(pid)
        return int(proc.create_time() * 1000), ""
    except psutil.NoSuchProcess:
        return None, CAUSE_PID_ABSENT
    except psutil.AccessDenied:
        return None, CAUSE_ACCESS_DENIED


def _process_create_time_ms(pid: Optional[int]) -> Optional[int]:
    """Readable-create-time-only view of :func:`_probe_create_time`.

    For callers that cannot act on the cause and only need "no usable time"
    (the pid re-anchor check in core.py). Absent and refused both read None
    here - that caller gives up either way.
    """
    create_ms, cause = _probe_create_time(pid)
    return None if cause else create_ms


def _liveness_reading(claim: Claim) -> tuple[bool, str]:
    """Return ``(verifiably_live, cause)`` for the claim's holder.

    ``cause`` is one of the CAUSE_* constants: the branch that decided.
    ``is_live`` is the bool view of this; the classifier carries the cause
    beside its verdict so callers can tell the ways a holder fails apart.
    """
    if claim.pid_unavailable:
        return False, CAUSE_PID_UNAVAILABLE
    if not is_same_machine(claim.host, claim.machine_id):
        return False, CAUSE_OFFHOST

    create_ms, cause = _probe_create_time(claim.pid)
    if cause:
        return False, cause

    # PID-reuse: the process currently holding this PID started AFTER the
    # claim was filed, so it is a different process. The original holder
    # died and the kernel handed the slot to someone else.
    if create_ms is not None and create_ms > claim.acquired_at:
        return False, CAUSE_PID_REUSE

    return True, CAUSE_LIVE


def is_live(claim: Claim) -> bool:
    """Return True iff the claim's holder is verifiably running.

    Returns False if:
      - The claim never recorded a pid (pid_unavailable, or pid is None).
      - The claim is on another machine (we cannot remotely verify).
      - The OS does not report claim.pid.
      - The holder process exists but refuses inspection (AccessDenied).
        Unreadable is NOT provably dead - see _probe_create_time - and
        classify reads that cause as SUSPECT so the claim is never stolen
        on pid evidence.
      - The OS-reported create_time for claim.pid is AFTER claim.acquired_at
        (PID reuse: a new process took over the slot).
    """
    return _liveness_reading(claim)[0]


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


def classify(
    claim: Claim,
    now: Optional[int] = None,
    *,
    pid_exclusive: Optional[bool] = None,
) -> ClaimState:
    """Compose is_live + is_expired into a state classification.

    A PID-liveness claim is STALE when the holder process is dead or replaced.
    A TTL claim within its window is LIVE when its pid is live, else SUSPECT
    (x-ba4b) - never reclaimable until the TTL actually expires.

    HYBRID liveness (ab-cc5553f2), corroborated: a TTL claim whose clock has
    lapsed is NOT unconditionally STALE - a live recorded pid keeps it LIVE
    only when that pid was PROVEN to be the holder session's own process at
    write time (``pid_provenance == "session-prover"``, verified against the
    process-tree prover when the claim was filed). A live pid under any other
    provenance - caller-supplied, resolved through an ambient harness marker,
    or a legacy claim with no field - falls to STALE at expiry, exactly as a
    pre-hybrid claim did. The corroboration is what keeps the arm load-bearing
    without letting a foreign process answer for the holder: a TTL is a lease,
    and a live foreign pid (the specimen: a chat app's app-server) must not
    make it permanent. ``is_live`` still guards host + pid-reuse
    (create_time < acquired_at).

    ``pid_exclusive`` is the second thing a prover-proven pid must be besides
    proven: the ONLY session it names (ab-6d5afbde). A prover can honestly
    resolve every session a daemon hosts to that daemon, so several distinct
    holders end up prover-proven onto one live pid; provenance alone then
    proves the DAEMON lives, never any one holder. When a caller that sees
    multiple claims (the reap sweep, which builds the property from the claim
    records it scans) passes ``False``, that pid can neither corroborate the
    lease (LIVE) nor prove the holder dead (STALE), so the claim reads SUSPECT
    and the roster/transcript instruments decide. ``None`` - unsupplied by
    every single-claim reader (acquire, status, the spawn guard) - is UNKNOWN,
    and unknown keeps the corroborated arm exactly as before: absent sibling
    evidence must not demote a claim.

    SUSPECT arm (x-ba4b): a TTL claim still inside its window whose recorded pid
    is NOT live reads SUSPECT, not LIVE. Dead-pid-but-unexpired is the respawned-
    worker case; the TTL keeps protecting the slot (acquire/dispatch refuse it
    like LIVE), but the distinct state lets init/dispatch branch. Only TTL expiry
    frees the claim (-> STALE). Mirrors ``claims.rs::classify``.

    SUSPECT also covers the unreadable holder on a PID-liveness claim:
    AccessDenied means the process EXISTS and refuses inspection (permission
    cannot be denied on a pid that is gone), so it is not a proof of death and
    must never free the claim on pid evidence.
    """
    live, cause = _liveness_reading(claim)
    if is_expired(claim, now=now):
        # HYBRID, corroborated: an expired clock does NOT imply a dead session,
        # but a live pid speaks for the holder only when it was prover-proven
        # at write time AND names no other holder. Anything else - an ambient
        # pid a foreign process answers for, a legacy claim that cannot prove
        # its pid, or a pid shared across distinct holders - loses the pid's
        # corroboration. The shared shape reads SUSPECT, not STALE: the pid
        # proves the daemon lives, which is neither holder-live nor holder-dead,
        # so the sweep's secondary instruments settle it.
        if claim.pid_provenance == "session-prover" and live:
            if pid_exclusive is False:
                return ClaimState.SUSPECT
            return ClaimState.LIVE
        return ClaimState.STALE
    if claim.expires_at is None:
        if live:
            return ClaimState.LIVE
        # Unreadable is not provably dead: never free a claim on pid evidence
        # we were refused.
        if cause == CAUSE_ACCESS_DENIED:
            return ClaimState.SUSPECT
        return ClaimState.STALE
    # TTL claim, not yet expired: live pid => LIVE, dead/replaced pid => SUSPECT
    # (TTL-protected, not stealable).
    return ClaimState.LIVE if live else ClaimState.SUSPECT


def classify_for_sweep(
    claim: Claim,
    now: Optional[int] = None,
    *,
    pid_exclusive: Optional[bool] = None,
) -> tuple[bool, str]:
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
    TTL whose pid is still live reads LIVE only when that pid was prover-proven
    (the corroborated hybrid arm in ``classify``), so the expiry arm never reaps
    a running local session - but it does reap an expired claim whose pid a
    foreign process merely answers for.

    ``pid_exclusive`` carries the sweep's sibling evidence into that arm
    (ab-6d5afbde): a prover-proven pid that names MORE THAN ONE distinct holder
    reads the expired claim SUSPECT rather than LIVE, so the sweep's
    roster/transcript instruments - not the shared daemon - decide. ``None``
    (single-claim callers, and any sweep that could not build the property)
    is unknown and keeps the claim's own evidence deciding, exactly as before.

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
    # own proof, so it reaches classify() even from a host we cannot verify -
    # but ONLY for a claim with no machine_id.
    #
    # That is exactly the row this arm exists for: written before the field
    # existed, identified by a hostname that MOVES, so it can never satisfy a
    # same-machine proof and stays unreapable for the life of the disk.
    #
    # A claim that DOES name another machine keeps the gate. classify()'s
    # corroborated hybrid arm reads an expired claim as LIVE when its pid is
    # live AND prover-proven, and that pid is only meaningful on the machine
    # that wrote it. Reaping from here would let
    # this host archive a claim its owner is still refreshing, and the next
    # reader would see the node free and staff a second worker onto it.
    unidentifiable = not claim.machine_id
    if not same_machine and not (unidentifiable and is_expired(claim, now=now)):
        return False, "offhost"
    state = classify(claim, now=now, pid_exclusive=pid_exclusive)
    if state is ClaimState.STALE:
        return True, ""
    # NO one-shot arm here, and the omission is deliberate. A `dispatch:<id>`
    # holder is `spawn-cli:<pid>`, one process that launches and exits, so a
    # dead pid inside its TTL looks like a pure wedge. But that TTL IS the
    # boot window: it is documented to outlive its spawner precisely so a
    # second dispatcher does not launch onto a node whose worker has not
    # reached `fno do target init` yet. A background sweep reaping it collapses
    # the dedup window and re-dispatches into the gap, and it also voids the
    # `dispatch:think:<node>:<reason>` tokens, which have no node claim behind
    # them at all. Expiry still frees these on schedule through the arm above.
    #
    # The wedge is cleared where it is actually observed instead: the spawn
    # guard re-probes its OWN reservation key at the moment it is about to
    # launch. See `_reclaim_if_provably_dead` in `fno.agents.cli`, which can
    # take that step safely because the node claim now covers the launch window.
    return False, "suspect" if state is ClaimState.SUSPECT else "live"


def is_provably_dead(claim: Claim, now: Optional[int] = None) -> bool:
    """Return True iff a claim's holder can be PROVEN dead from this host.

    A thin bool-only view of :func:`classify_for_sweep` for a caller that
    only needs the yes/no answer.
    """
    return classify_for_sweep(claim, now=now)[0]
