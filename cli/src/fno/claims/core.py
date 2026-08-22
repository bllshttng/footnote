"""High-level claim verbs.

Seven operations on top of io + staleness:

    acquire_claim     - try to take a claim; idempotent re-acquire,
                        stale recovery, live-other detection.
    release_claim     - drop a claim we own.
    refresh_claim     - extend TTL on a claim we own (no-op for PID-liveness).
    claim_status      - inspect a single key.
    list_claims       - enumerate all live (and optionally stale) claims.
    force_release_claim - administrative override, always succeeds.
    reap_dead_claims  - archive every provably-dead claim (GC).

Every state-changing verb appends an audit event to ``.fno/events.jsonl``
through the typed builders in :mod:`fno.claims.events`. Audit-trail
writes are best-effort: the YAML lock file write is authoritative.
"""
from __future__ import annotations

import os
import socket
from pathlib import Path
from typing import Any, Callable, Optional

from urllib.parse import quote as _url_quote

from .events import (
    emit_claim_acquired,
    emit_claim_force_overridden,
    emit_claim_idempotent_reacquired,
    emit_claim_reap_swept,
    emit_claim_reaped,
    emit_claim_refreshed,
    emit_claim_rebound,
    emit_claim_released,
    emit_claim_stale_reclaimed,
)
from .hostid import is_same_machine, machine_id
from .io import (
    ClaimAlreadyHeld,
    ClaimCorrupted,
    ClaimGoneAway,
    archive_claim,
    atomic_create_exclusive,
    claim_path,
    claims_dir,
    decode_key,
    dedup_claims_roots,
    global_claims_root,
    read_claim_file,
    serialize_claim,
)
from .staleness import classify, classify_for_sweep, is_expired, is_live, now_ms
from .self_identity import resolve_self_identity
from ..mutex import acquire_dir_mutex, release_dir_mutex
from .types import (
    MAX_ENCODED_FILENAME_BYTES,
    MAX_KEY_LENGTH,
    MAX_TTL_MS,
    MIN_TTL_MS,
    Claim,
    ClaimState,
)


class ClaimHeldByOther(Exception):
    """A live claim is held by a different holder."""

    def __init__(self, holder: str, pid: int, host: str, key: str) -> None:
        self.holder = holder
        self.pid = pid
        self.host = host
        self.key = key
        super().__init__(f"claim {key!r} held by {holder} (pid={pid}, host={host})")


class HolderMismatch(Exception):
    """release/refresh called with a different holder than the existing claim."""

    def __init__(self, expected: str, actual: str, key: str) -> None:
        self.expected = expected
        self.actual = actual
        self.key = key
        super().__init__(f"claim {key!r}: holder mismatch (expected {expected!r}, got {actual!r})")


class ClaimValidationError(ValueError):
    """Inputs to a verb failed validation (ttl out of range, key too long, ...)."""


class ClaimContended(Exception):
    """acquire_claim/refresh_claim gave up after ACQUIRE_MAX_ATTEMPTS
    contention retries on the same key's recovery mutex.

    A distinct type from ClaimHeldByOther (a live claim is held by someone
    else) even though a caller usually treats both the same way ("can't have
    this one right now, retry later") - the recovery mutex being contended
    says nothing about who, if anyone, ends up holding the claim. Callers
    that only catch this narrow type cannot accidentally reclassify an
    unrelated RuntimeError raised deeper in the call stack (pydantic,
    resolve_self_identity, serialize_claim, ...) as contention.

    Callers of acquire_claim/refresh_claim should catch this ALONGSIDE
    ClaimHeldByOther, not instead of it - an except clause naming only one of
    the two lets the other escape uncaught.
    """


# Every acquire_claim/refresh_claim caller that treats "someone else has this
# right now" and "the recovery mutex is too busy to tell" the same way
# ("can't have this one right now, retry/skip") should catch this tuple
# instead of hand-rolling `except (ClaimHeldByOther, ClaimContended):` plus a
# restated comment at each site. A caller that needs to read `.holder`/`.pid`/
# `.host` (ClaimHeldByOther-only attributes) still needs its own separate
# `except ClaimHeldByOther as exc:` block ahead of this one.
CLAIM_UNAVAILABLE = (ClaimHeldByOther, ClaimContended)


class RebindRefused(Exception):
    """``compare_and_rebind`` refused to move the claim (fail-closed, x-2ccd).

    Native resume never silently believes it owns a claim: every path that is
    not an affirmative same-holder local rebind raises this with a named
    reason. Carries the observed ``state``/``holder``/``pid`` so the caller can
    render a loud, specific refusal.
    """

    def __init__(
        self,
        reason: str,
        *,
        state: Optional[str] = None,
        holder: Optional[str] = None,
        pid: Optional[int] = None,
    ) -> None:
        self.reason = reason
        self.state = state
        self.holder = holder
        self.pid = pid
        super().__init__(reason)


# Re-export low-level exceptions so callers can ``from fno.claims import ClaimGoneAway``.
__all__ = [
    "CLAIM_UNAVAILABLE",
    "ClaimAlreadyHeld",
    "ClaimContended",
    "ClaimCorrupted",
    "ClaimGoneAway",
    "ClaimHeldByOther",
    "ClaimValidationError",
    "HolderMismatch",
    "RebindRefused",
    "acquire_claim",
    "claim_status",
    "compare_and_rebind",
    "force_release_claim",
    "list_claims",
    "list_claims_with_counts",
    "reap_dead_claims",
    "sweep_verdict",
    "refresh_claim",
    "release_claim",
]


def _validate_inputs(
    key: str,
    holder: str,
    ttl_ms: Optional[int],
) -> None:
    if not key:
        raise ClaimValidationError("key must be non-empty")
    if len(key) > MAX_KEY_LENGTH:
        raise ClaimValidationError(
            f"key length {len(key)} exceeds MAX_KEY_LENGTH={MAX_KEY_LENGTH}"
        )
    # Raw length passing MAX_KEY_LENGTH does not guarantee the encoded
    # filename fits the filesystem's 255-byte name limit. Check the
    # URL-encoded form explicitly: keys with many reserved characters
    # (slashes, colons) expand up to 3x.
    encoded_len = len(_url_quote(key, safe="").encode("utf-8"))
    if encoded_len > MAX_ENCODED_FILENAME_BYTES:
        raise ClaimValidationError(
            f"URL-encoded key length {encoded_len} exceeds "
            f"MAX_ENCODED_FILENAME_BYTES={MAX_ENCODED_FILENAME_BYTES}"
        )
    if not holder:
        raise ClaimValidationError("holder must be non-empty")
    if ttl_ms is not None and not (MIN_TTL_MS <= ttl_ms <= MAX_TTL_MS):
        raise ClaimValidationError(
            f"ttl_ms={ttl_ms} out of range [{MIN_TTL_MS}, {MAX_TTL_MS}]"
        )


def _make_claim(
    key: str,
    holder: str,
    ttl_ms: Optional[int],
    reason: Optional[str],
    metadata: Optional[dict[str, Any]],
    pid: Optional[int],
    host: Optional[str],
    harness: Optional[str] = None,
) -> Claim:
    acquired = now_ms()
    return Claim(
        key=key,
        holder=holder,
        acquired_at=acquired,
        expires_at=(acquired + ttl_ms) if ttl_ms is not None else None,
        pid=pid if pid is not None else os.getpid(),
        host=host if host is not None else socket.gethostname(),
        # Liveness compares THIS, not host: a name that flips mid-session made a
        # live holder read cross-host, then stale, then stealable. Additive, so a
        # pre-change reader still reads host and behaves exactly as today. `or
        # None` omits the field when no stable id exists rather than recording a
        # hostname readers would trust as authoritative.
        machine_id=machine_id() or None,
        reason=reason,
        # x-3e70: tag the claim with the acquiring harness so the dispatch guard
        # can read a foreign owner off the claim. This is the PRODUCTION writer
        # (`fno agents claim` forwards to this Python CLI), kept in lockstep with the
        # Rust make_claim resolver via the shared harness_identity markers.
        # An explicit `harness` wins over ambient resolution so callers can pin
        # the owning harness deterministically.
        #
        # Resolution happens in the CALLER, before the recovery mutex, never
        # here: the owned path walks the process tree, and this function runs
        # inside the critical section every other acquirer waits on (x-20f1).
        harness=harness,
        metadata=metadata or {},
    )


def acquire_claim(
    key: str,
    holder: str,
    *,
    reason: Optional[str] = None,
    ttl_ms: Optional[int] = None,
    metadata: Optional[dict[str, Any]] = None,
    pid: Optional[int] = None,
    host: Optional[str] = None,
    harness: Optional[str] = None,
    root: Optional[Path] = None,
    _attempt: int = 0,
) -> Claim:
    """Try to acquire a claim on ``key`` for ``holder``.

    Resolution order when the path already exists:
      1. Existing holder == requested holder => idempotent re-acquire:
         rewrite the file with refreshed pid/host/acquired_at; emit
         ``claim_idempotent_reacquired``.
      2. Existing claim is stale (dead PID or TTL expired) => recovery:
         archive to ``.expired/``, retry exclusive-create once. On
         retry-EEXIST: re-read; whoever won, return result of step 1 or
         raise ClaimHeldByOther.
      3. Existing claim is live and held by another => raise
         ClaimHeldByOther.

    Inputs are validated up front (key length, ttl bounds, non-empty
    holder). Validation failures raise ClaimValidationError before any
    filesystem write so the lock dir is not polluted with half-bad files.

    ``_attempt`` is internal bookkeeping only (never pass it): each
    contention/race branch recurses through ``_retry()``, which counts
    attempts and raises ``ClaimContended`` after ``ACQUIRE_MAX_ATTEMPTS``
    rather than recursing unbounded, mirroring Rust's bounded
    ``ACQUIRE_MAX_ATTEMPTS`` for-loop (crates/fno-agents/src/claims.rs).
    """
    _validate_inputs(key, holder, ttl_ms)
    path = claim_path(key, root=root)
    # Unconditional initial value (each branch below reassigns its own):
    # gives _release_and_retry's `nonlocal` an unambiguous prior binding
    # rather than relying on mypy tracing every conditional branch.
    acquired_lock = False

    def _retry() -> Claim:
        # Every contention/race branch below re-dispatches by recursing with
        # the exact same arguments - one definition instead of the same
        # 9-line call restated at each of the seven sites that need it.
        if _attempt + 1 >= ACQUIRE_MAX_ATTEMPTS:
            raise ClaimContended(
                f"acquire_claim gave up after {ACQUIRE_MAX_ATTEMPTS} "
                f"contention retries on {key!r}"
            )
        return acquire_claim(
            key,
            holder,
            reason=reason,
            ttl_ms=ttl_ms,
            metadata=metadata,
            pid=pid,
            host=host,
            harness=harness,
            root=root,
            _attempt=_attempt + 1,
        )

    def _release_and_retry() -> Claim:
        # `return _retry()` evaluates the recursive acquire_claim() call
        # BEFORE this frame's own `finally` runs (Python evaluates a
        # return expression, then unwinds through finally). Recursing while
        # `acquired_lock` is still True would have the recursive call poll
        # for the SAME per-key recovery mutex this frame is still sitting
        # on if it lands back in a mutex-taking branch - self-contention
        # that only resolves via ACQUIRE_MAX_ATTEMPTS exhaustion instead of
        # the near-instant re-dispatch (e.g. ClaimHeldByOther) it should.
        # Release first, from whichever branch currently holds it.
        nonlocal acquired_lock
        if acquired_lock:
            release_dir_mutex(recovery_lock, recovery_token)
            acquired_lock = False
        return _retry()

    # Resolve the harness ONCE, here, outside every mutex below. The owned path
    # walks the process tree, and `_make_claim` is called from inside the
    # recovery critical section that other acquirers are polling on: doing it
    # there put a process walk in every waiter's path. Resolving up front also
    # makes the first write and any refresh agree on one value rather than
    # re-resolving under contention (x-20f1).
    if harness is None:
        harness = resolve_self_identity().harness

    new_claim = _make_claim(key, holder, ttl_ms, reason, metadata, pid, host, harness)
    payload = serialize_claim(new_claim)

    try:
        atomic_create_exclusive(path, payload)
        emit_claim_acquired(new_claim)
        return new_claim
    except ClaimAlreadyHeld:
        pass

    # Path exists; classify the existing holder.
    try:
        existing = read_claim_file(path)
    except ClaimGoneAway:
        # Disappeared between collision and read - someone else released
        # while we were looking. Recurse once; if still racy, surface it.
        return _retry()

    if existing.holder == holder:
        # Idempotent re-acquire: refresh pid/host/acquired_at. Take the same
        # per-key recovery mutex reap_dead_claims() holds while it re-verifies
        # and archives this exact file - without it, a respawned worker could
        # rewrite the file as live in the gap between reap's re-verify and its
        # archive_claim() call, and reap would archive the fresh write instead
        # of the dead one it proved. Contention handling mirrors the
        # stale-reclaim branch below (steal a corpse or wait briefly, then
        # recurse) rather than a bare retry loop, which would spin straight
        # into RecursionError against a genuinely live holder.
        #
        # This mkdir-path-build + acquired_lock/recovery_token + try/finally
        # shape repeats at 6 sites in this file (here, the stale-reclaim
        # branch below, compare_and_rebind, refresh_claim, force_release_claim,
        # and reap_dead_claims). acquire_dir_mutex already collapsed the inner
        # mkdir/steal/wait logic each site used to hand-roll; a further
        # `with recovery_mutex(path) as token:` context manager could
        # collapse this outer bookkeeping too, but each site's body differs
        # enough (recurse vs. raise vs. continue on timeout) that it was
        # judged a separate, larger refactor rather than folded into this
        # PR's mutex-consolidation and reap-hardening scope.
        recovery_lock = path.with_name(path.name + RECOVERY_LOCK_SUFFIX)
        acquired_lock = False
        recovery_token = ""
        try:
            token = acquire_dir_mutex(
                recovery_lock, _RECOVERY_LOCK_MAX_WAIT_S, poll_s=_RECOVERY_LOCK_POLL_INTERVAL_S
            )
            if token is None:
                return _retry()
            recovery_token = token
            acquired_lock = True

            # Re-verify under the mutex: a concurrent stale-reclaim or reap
            # archive could have completed between our initial unlocked read
            # (above) and acquiring this lock, installing a different holder
            # (or nothing) at this path. Recurse rather than blindly
            # overwrite whatever is there now - the recursion re-reads and
            # re-dispatches to whichever branch the fresh state calls for.
            try:
                fresh_existing = read_claim_file(path)
            except ClaimGoneAway:
                return _release_and_retry()
            if fresh_existing.holder != holder:
                return _release_and_retry()

            refreshed = _make_claim(key, holder, ttl_ms, reason, metadata, pid, host, harness)
            _atomic_replace(path, serialize_claim(refreshed))
            emit_claim_idempotent_reacquired(refreshed, previous=fresh_existing)
            return refreshed
        finally:
            if acquired_lock:
                release_dir_mutex(recovery_lock, recovery_token)

    # Stale? Try recovery under a mkdir-based recovery mutex so the archive +
    # recreate steps are serialized across concurrent workers. Without the
    # mutex, two workers can both observe a stale file, both archive (one
    # actually moves, one no-ops), and both successfully create the new lock
    # in the gap between archive-and-create.
    if not _existing_is_live(existing):
        recovery_lock = path.with_name(path.name + RECOVERY_LOCK_SUFFIX)
        acquired_lock = False
        recovery_token = ""
        try:
            # Another worker may be doing recovery -- or died holding the
            # mutex. acquire_dir_mutex steals a corpse (age-based,
            # rename-atomic) so a killed recoverer cannot brick this key
            # forever, else polls briefly. Either outcome on timeout means
            # recurse from the top: the recovering worker will either
            # succeed (we then see live-other) or fail (we get another shot).
            token = acquire_dir_mutex(
                recovery_lock, _RECOVERY_LOCK_MAX_WAIT_S, poll_s=_RECOVERY_LOCK_POLL_INTERVAL_S
            )
            if token is None:
                return _retry()
            recovery_token = token
            acquired_lock = True

            # Inside the recovery mutex: verify the existing claim is still
            # what we read (a fast-moving releaser could have unlinked it).
            try:
                existing = read_claim_file(path)
            except ClaimGoneAway:
                # File vanished while we held the recovery lock - someone
                # released cleanly. Try to create at the empty path; if a
                # third worker races into create between our gone-away
                # detection and this call, recurse rather than raising the
                # low-level ClaimAlreadyHeld out of acquire_claim.
                try:
                    atomic_create_exclusive(path, payload)
                except ClaimAlreadyHeld:
                    return _release_and_retry()
                emit_claim_acquired(new_claim)
                return new_claim

            if existing.holder == holder:
                # Raced into the idempotent path while we were grabbing the lock.
                refreshed = _make_claim(key, holder, ttl_ms, reason, metadata, pid, host, harness)
                _atomic_replace(path, serialize_claim(refreshed))
                emit_claim_idempotent_reacquired(refreshed, previous=existing)
                return refreshed

            if _existing_is_live(existing):
                # Raced - now it's live. Fall through to ClaimHeldByOther.
                raise ClaimHeldByOther(
                    holder=existing.holder,
                    pid=existing.pid,
                    host=existing.host,
                    key=key,
                )

            # Still stale; do the archive + recreate atomically (under the mutex).
            archive_claim(path, ts_ms=now_ms())
            try:
                atomic_create_exclusive(path, payload)
            except ClaimAlreadyHeld:
                # A top-level creator won the empty path in the gap between our
                # archive and this create (the top-level O_EXCL does not take the
                # recovery mutex). Recurse to re-read and classify rather than
                # letting the low-level ClaimAlreadyHeld escape acquire_claim's
                # return-or-ClaimHeldByOther contract; the recursion sees the new
                # live holder and raises ClaimHeldByOther.
                return _release_and_retry()
            emit_claim_stale_reclaimed(new_claim, previous=existing)
            return new_claim
        finally:
            if acquired_lock:
                release_dir_mutex(recovery_lock, recovery_token)

    # Live and not us => block.
    raise ClaimHeldByOther(
        holder=existing.holder,
        pid=existing.pid,
        host=existing.host,
        key=key,
    )


def _rebound_claim(
    existing: Claim,
    new_pid: int,
    ttl_ms: Optional[int],
    *,
    new_holder: Optional[str] = None,
    new_reason: Optional[str] = None,
    new_harness: Optional[str] = None,
    new_metadata: Optional[dict] = None,
    keep_acquired_at: bool = False,
) -> Claim:
    """A rebound claim: identity fields preserved, process anchor + lease fresh.

    The native-resume rebind keeps ``key``/``holder``/``reason``/``metadata``/
    ``harness`` (one target attempt retains one symbolic owner) and rewrites
    only ``pid``/``host``/``machine_id``/``acquired_at``/``expires_at``. A
    PID-liveness claim stays PID-liveness (``ttl_ms`` None and no prior
    ``expires_at``); a TTL claim refreshes to ``now + prior_window`` so the
    deadline never compounds across rebinds (the renew() lesson).

    ``new_holder`` is the one exception to "identity preserved", and only the
    dispatch handover passes it: the spawn side and the worker side of one
    launch are genuinely different names for one piece of work. Its caller
    proved the prior holder first; see :func:`compare_and_rebind`.

    ``new_reason`` and ``new_harness`` travel with it. A handover changes WHO
    owns the claim, so keeping the spawner's reason and harness tag would leave
    the claim describing the wrong owner - and the init hook passes a PROVEN
    harness precisely so a session that inherited a foreign marker does not
    mislabel its claim. Both default to None, which preserves the existing
    value, so every same-holder rebind is unchanged.

    ``keep_acquired_at`` is for the renewal RE-ANCHOR, which repairs the process
    anchor rather than acquiring anew. Two things depend on it. The do
    provenance row keys ``started_at`` on ``acquired_at``, so moving it makes
    the release stamp open a SECOND row instead of closing the one this claim
    opened. And PID-reuse detection compares ``create_time(pid)`` against it, so
    holding it still refuses an anchor whose session began AFTER the claim -
    which is the cross-session takeover a re-anchor must never perform.
    """
    # TWO CLOCKS, and conflating them froze the lease. `acquired` is the record
    # of when this claim began; the DEADLINE always runs from now. Deriving the
    # deadline from a held `acquired` made a re-anchoring refresh extend the
    # claim by zero, and on a short window it wrote a deadline in the PAST, so
    # the heartbeat drove its own claim from suspect straight to stale.
    now = now_ms()
    acquired = existing.acquired_at if keep_acquired_at else now
    if ttl_ms is not None:
        expires_at: Optional[int] = now + ttl_ms
    elif existing.expires_at is not None:
        expires_at = now + max(existing.expires_at - existing.acquired_at, MIN_TTL_MS)
    else:
        expires_at = None
    return Claim(
        schema_version=existing.schema_version,
        key=existing.key,
        holder=new_holder or existing.holder,
        acquired_at=acquired,
        expires_at=expires_at,
        pid=new_pid,
        host=socket.gethostname(),
        machine_id=machine_id() or None,
        reason=new_reason if new_reason is not None else existing.reason,
        harness=new_harness if new_harness is not None else existing.harness,
        metadata=new_metadata if new_metadata else existing.metadata,
    )


def compare_and_rebind(
    key: str,
    expected_holder: str,
    *,
    new_holder: Optional[str] = None,
    new_reason: Optional[str] = None,
    new_harness: Optional[str] = None,
    new_metadata: Optional[dict] = None,
    new_pid: Optional[int] = None,
    ttl_ms: Optional[int] = None,
    root: Optional[Path] = None,
    emit: bool = True,
    fno_id: Optional[str] = None,
    harness_tag: Optional[str] = None,
    harness_session_id: Optional[str] = None,
) -> tuple[Claim, str]:
    """Atomically rebind a same-holder LOCAL claim whose prior PID is dead.

    The native-resume primitive (x-2ccd). A resumed durable session proves it
    owns a target claim by matching the symbolic holder AND showing the
    recorded PID is dead on THIS machine, then takes a fresh PID + lease.
    Distinct from ``acquire_claim``, which overwrites a same-holder claim even
    when its PID is still live: two concurrent processes of one durable
    conversation could then steal the claim back and forth.

    Under the per-claim recovery mutex (the same one ``acquire_claim`` uses for
    stale recovery), re-read and re-classify, then:

    - prior PID still LIVE, == this pid  -> idempotent lease refresh;
    - prior PID still LIVE, != this pid  -> ``RebindRefused`` (concurrent writer);
    - SUSPECT/STALE but off-host          -> ``RebindRefused`` (death unproven);
    - holder changed / claim gone / free / corrupt -> ``RebindRefused``;
    - local same-holder, prior PID dead  -> atomically rebind to ``new_pid``.

    Never creates a missing/free claim and never archives another holder (that
    is the explicit ``fno do target start`` successor path). Emits ``claim_rebound``;
    raises ``RebindRefused`` on any refusal.

    ``new_holder`` moves the claim to a DIFFERENT holder on proof of the prior
    one (x-cd1e). The dispatch handover needs it: ``fno agents spawn --node``
    takes the node claim before the worker exists, and the worker's own
    ``fno do target init`` must then take it over rather than find it held and
    abort. ``acquire_claim`` cannot do this - it raises ``ClaimHeldByOther`` for
    a different holder on a live claim - and the same-holder rebind above cannot
    either, because the two ends genuinely have different names.

    Proof is what makes the move safe, and it is the SAME proof the same-holder
    path already demands: the caller must name the exact prior holder, and this
    re-reads under the mutex and refuses on any mismatch. The spawn-side holder
    is worker-specific and reaches only that worker (exported into its
    environment), so naming it is evidence of being the intended successor
    rather than a bystander who guessed a key. A live prior PID that is not this
    one still refuses as a concurrent writer, so the move never yanks a running
    owner.

    Omitting it preserves the holder, which is every pre-existing caller.

    Returns ``(claim, mode)`` where mode is ``"rebind"`` (a dead prior PID was
    rebound), ``"idempotent"`` (a live same-PID lease refresh), or ``"handover"``
    (a named prior holder was replaced by ``new_holder``).
    """
    _validate_inputs(key, expected_holder, ttl_ms)
    path = claim_path(key, root=root)
    npid = new_pid if new_pid is not None else os.getpid()
    # Resolved BEFORE the recovery mutex below, for the same reason as
    # `acquire_claim`: the owned path walks the process tree, and everything
    # after the lock runs while other callers poll on it (x-20f1).
    resolved_harness = (
        new_harness if new_harness is not None else resolve_self_identity().harness
    )
    recovery_lock = path.with_name(path.name + RECOVERY_LOCK_SUFFIX)
    acquired_lock = False
    recovery_token = ""
    try:
        # A peer may be mid-recovery, or a recoverer died holding the mutex.
        # acquire_dir_mutex steals a corpse so a killed peer cannot brick the
        # rebind, else polls until timeout - a mutex that clears well inside
        # the window (acquire_claim/reap's own archive-then-recreate is a
        # few-ms critical section) now lets the rebind succeed instead of
        # refusing on a steal-attempt-then-give-up basis. compare_and_rebind
        # is a one-shot verb (unlike acquire_claim/refresh_claim it does not
        # recurse), so a genuine timeout here still refuses rather than
        # retrying from the top.
        token = acquire_dir_mutex(
            recovery_lock, _RECOVERY_LOCK_MAX_WAIT_S, poll_s=_RECOVERY_LOCK_POLL_INTERVAL_S
        )
        if token is None:
            raise RebindRefused(
                "claim recovery mutex busy; retry the resume bind",
                state=None,
            )
        recovery_token = token
        acquired_lock = True

        # Inside the mutex: re-read + classify before any mutation.
        try:
            existing = read_claim_file(path)
        except ClaimGoneAway:
            raise RebindRefused(
                "claim vanished; this target no longer owns the node "
                "(use `fno do target start` to reclaim)",
                state="free",
            )
        except ClaimCorrupted:
            raise RebindRefused(
                "claim corrupted; cannot verify ownership (use `fno agents claim release --force`)",
                state="corrupted",
            )

        if existing.holder != expected_holder:
            raise RebindRefused(
                f"holder mismatch: expected {expected_holder!r}, claim held by "
                f"{existing.holder!r} (pid={existing.pid})",
                state=classify(existing).value,
                holder=existing.holder,
                pid=existing.pid,
            )

        state = classify(existing)
        # ONLY a launch-window holder may be replaced. Naming the prior holder
        # is the proof, and for `spawn-handover:<worker>` that proof is real: it
        # is minted per worker and travels only in that worker's environment.
        # Every other holder is PUBLISHED by `fno agents claim status`, so without this
        # anyone could read a holder off the store and hand the node to
        # themselves - taking a running owner's claim, which `ClaimHeldByOther`
        # had always refused.
        #
        # Computed ONCE and applied at all three rebind sites. Gating only the
        # LIVE branch would leave the rename reachable through the idempotent
        # and dead-owner paths, which is the same rule on one of three paths.
        handover_allowed = bool(
            new_holder
            and new_holder != existing.holder
            and existing.holder.startswith(HANDOVER_HOLDER_PREFIX)
        )
        if new_holder and new_holder != existing.holder and not handover_allowed:
            # REFUSE, never fall through. Gating only the RENAME left this call
            # dropping into the same-holder rebind below, which rewrote the
            # victim's pid/host/expires_at and republished their claim as LIVE
            # under THIS process. That is worse than the takeover the gate was
            # added to stop: the claim then reads live to every dispatcher and
            # `sweep_verdict` short-circuits on LIVE, so nothing can reap it.
            raise RebindRefused(
                f"holder {existing.holder!r} is not a launch-window holder; "
                "only a spawn-side handover claim can be taken over",
                state=classify(existing).value,
                holder=existing.holder,
                pid=existing.pid,
            )
        effective_new_holder = new_holder if handover_allowed else None
        # The reason and the harness tag describe the OWNER, so they travel with
        # the rename or not at all. Applying them to a refused handover let a
        # caller rewrite another holder's fields while leaving the holder alone.
        effective_new_reason = new_reason if handover_allowed else None
        effective_new_metadata = new_metadata if handover_allowed else None
        # A handover with no PINNED harness resolves one from the ambient
        # markers, exactly as `_make_claim` does on the ordinary acquire path.
        # Preserving the spawner's tag instead left a claude worker under a
        # codex king reading as codex for the life of the claim, and that tag
        # flows on into the do provenance row. The init hook omits --harness
        # whenever its owned-identity probe fails, so this is not a rare path.
        effective_new_harness = resolved_harness if handover_allowed else None
        if state == ClaimState.LIVE and handover_allowed:
            # A HANDOVER, and a live prior pid does not refuse it. The
            # concurrent-writer rule below protects one symbolic owner from two
            # of its own processes, which is a different situation: here the
            # caller named a DIFFERENT prior holder exactly, and that holder
            # exists only to be handed over.
            #
            # A live prior pid is in fact the NORMAL case on the blocking
            # substrates. `fno agents spawn --substrate headless` (and `--once`)
            # stays in dispatch_spawn for the worker's whole run, so the spawner
            # is still alive when the worker reaches `fno do target init`. Refusing
            # there left the worker unclaimed for the full lease, which is the
            # free-read this whole change exists to close, reintroduced on the
            # one substrate that blocks.
            rebound = _rebound_claim(
                existing, npid, ttl_ms, new_holder=effective_new_holder,
                new_reason=effective_new_reason, new_harness=effective_new_harness,
                new_metadata=effective_new_metadata,
            )
            _atomic_replace(path, serialize_claim(rebound))
            if emit:
                emit_claim_rebound(
                    rebound,
                    previous_pid=existing.pid,
                    previous_state=state.value,
                    mode="handover",
                    fno_id=fno_id,
                    harness=harness_tag,
                    harness_session_id=harness_session_id,
                )
            return rebound, "handover"
        if state == ClaimState.LIVE:
            if existing.pid == npid:
                # Idempotent: already bound to this process; refresh lease only.
                rebound = _rebound_claim(
                existing, npid, ttl_ms, new_holder=effective_new_holder,
                new_reason=effective_new_reason, new_harness=effective_new_harness,
                new_metadata=effective_new_metadata,
            )
                _atomic_replace(path, serialize_claim(rebound))
                if emit:
                    emit_claim_rebound(
                        rebound,
                        previous_pid=existing.pid,
                        previous_state=state.value,
                        mode="idempotent",
                        fno_id=fno_id,
                        harness=harness_tag,
                        harness_session_id=harness_session_id,
                    )
                return rebound, "idempotent"
            # A DIFFERENT live PID holds this same durable session: a concurrent
            # writer of one conversation. Refuse rather than yank the claim.
            raise RebindRefused(
                f"concurrent writer: claim held by live pid {existing.pid}, "
                f"this pid is {npid}; refusing to rebind a live owner",
                state="live",
                pid=existing.pid,
            )

        # SUSPECT or STALE. Rebind only when the dead owner is on THIS machine
        # (death proven locally). Off-host/unverifiable -> refuse: Footnote
        # cannot prove the owner is dead, so rebind would be a foreign takeover.
        if not is_same_machine(existing.host, existing.machine_id):
            raise RebindRefused(
                "owner is off-host or machine identity is unverifiable; "
                "death unproven, will not rebind a foreign claim",
                state=state.value,
                pid=existing.pid,
            )

        # Local same-holder, prior PID dead: the resume rebind.
        rebound = _rebound_claim(
                existing, npid, ttl_ms, new_holder=effective_new_holder,
                new_reason=effective_new_reason, new_harness=effective_new_harness,
                new_metadata=effective_new_metadata,
            )
        _atomic_replace(path, serialize_claim(rebound))
        if emit:
            emit_claim_rebound(
                rebound,
                previous_pid=existing.pid,
                previous_state=state.value,
                mode="handover" if handover_allowed else "rebound",
                fno_id=fno_id,
                harness=harness_tag,
                harness_session_id=harness_session_id,
            )
        # A rename applied here is a HANDOVER, whatever the prior state was.
        # This is in fact the dominant real case: on the pane substrate the
        # spawner's pid is already dead when the worker reaches `target init`,
        # so the claim reads SUSPECT and lands on this branch, not the LIVE one
        # above. Reporting `rebound` made `acquire --handover-from` treat the
        # successful takeover as a decline, fall through, and write the claim a
        # second time - and labelled a holder change as a resume in the event.
        return rebound, ("handover" if handover_allowed else "rebound")
    finally:
        if acquired_lock:
            release_dir_mutex(recovery_lock, recovery_token)


#: Holder prefix marking a claim taken by `fno agents spawn --node` on behalf of
#: a worker that does not exist yet. The handover branch above accepts ONLY this
#: prefix as a replaceable prior holder, which is why the constant lives here
#: rather than in the command module that re-exports it.
#:
#: WHAT THIS IS NOT: a secret. `fno agents claim status` publishes every holder, so
#: naming one back proves nothing about who is asking. What the prefix restricts
#: is the BLAST RADIUS - only a launch-window claim can be taken over this way,
#: never a working session's `target-session:` claim, which `ClaimHeldByOther`
#: still protects. The window is TTL-bound (`HANDOVER_TTL`), so the exposure is
#: bounded to it, and before this change that same window carried NO claim at
#: all. Closing it properly needs a secret the worker alone holds, which is its
#: own change.
HANDOVER_HOLDER_PREFIX = "spawn-handover:"

#: Suffix of the per-claim recovery mutex directory. One definition: this
#: string was written out at six call sites, and a seventh (the dispatch
#: guard's targeted recovery) is what made the duplication worth collapsing.
#: A caller that spells it differently takes a DIFFERENT lock and silently
#: serializes against nobody.
RECOVERY_LOCK_SUFFIX = ".recovery.d"

_RECOVERY_LOCK_POLL_INTERVAL_S = 0.02
_RECOVERY_LOCK_MAX_WAIT_S = 5.0

# Mirrors Rust's ACQUIRE_MAX_ATTEMPTS (crates/fno-agents/src/claims.rs):
# acquire_claim/refresh_claim recurse on contention instead of Rust's bounded
# for-loop, so an attempt counter caps the recursion depth the same way.
ACQUIRE_MAX_ATTEMPTS = 5


def _existing_is_live(existing: Claim) -> bool:
    """Authoritative acquire/recovery liveness predicate.

    Delegates to ``classify`` so the mutex honors the SAME hybrid TTL-or-pid
    liveness as the selection/status reads (ab-cc5553f2): an expired TTL claim
    whose recorded pid is alive on this host is LIVE and must NOT be reclaimed
    by a peer (otherwise a suspended-but-alive session's node is stolen). One
    predicate means acquire and ``status``/``list`` can never diverge.

    SUSPECT (x-ba4b) counts as live here: a TTL-unexpired claim with a dead pid
    is a respawned worker's protected slot, so acquire must refuse it exactly
    like LIVE (never steal). Only TTL expiry (-> STALE) makes it reclaimable.
    """
    return classify(existing) in (ClaimState.LIVE, ClaimState.SUSPECT)


def _atomic_replace(path: Path, content: str) -> None:
    """Replace the file at path with content via write-temp + rename.

    Used by idempotent re-acquire and refresh - both legitimately overwrite
    an existing claim with new contents under the same holder. The temp
    file goes in the same directory so the rename is atomic on POSIX.

    Cleans up the tmp file on any failure between write and rename so a
    partial replace cannot leave orphan tmp files in the claims directory.
    """
    tmp = path.with_suffix(path.suffix + f".tmp.{os.getpid()}")
    try:
        tmp.write_text(content, encoding="utf-8")
        os.rename(str(tmp), str(path))
    except Exception:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise


def release_claim(
    key: str,
    holder: str,
    *,
    strict: bool = False,
    root: Optional[Path] = None,
) -> Optional["Claim"]:
    """Release a claim we hold.

    Behavior:
      - No file present: silent success (the claim is already released).
      - File present, our holder: unlink + emit ``claim_released``.
      - File present, different holder: silent success unless ``strict``
        (then raise HolderMismatch). Releases are idempotent in the common
        case; strict mode is for explicit "this MUST be ours" callers.
      - File present but corrupted: silent success (treat as released).

    The duration_held_ms field in the audit event is best-effort: read from
    acquired_at minus now. If the file disappears between read and unlink,
    that race is benign (another caller released).

    Returns the released ``Claim`` (carrying ``acquired_at``) on a real
    release, or ``None`` when nothing was released (already gone, holder
    mismatch, corrupted) - so a caller can stamp a window bounded by the
    claim's own acquire time without re-reading the file.
    """
    if not key or not holder:
        raise ClaimValidationError("key and holder must be non-empty")

    path = claim_path(key, root=root)
    if not path.exists():
        return None

    try:
        existing = read_claim_file(path)
    except ClaimGoneAway:
        return None
    except ClaimCorrupted:
        # Corrupted file: we cannot verify ownership. Conservative default
        # is to leave it for force_release. strict mode surfaces the issue.
        if strict:
            raise
        return None

    if existing.holder != holder:
        if strict:
            raise HolderMismatch(expected=holder, actual=existing.holder, key=key)
        return None

    duration_ms = max(0, now_ms() - existing.acquired_at)
    try:
        path.unlink()
    except FileNotFoundError:
        return None
    emit_claim_released(existing, duration_ms=duration_ms)
    return existing


def _reanchor_pid_for(existing: Claim) -> Optional[int]:
    """The durable pid a renewal should re-anchor EXISTING to, or None.

    Mirrors ``renew`` in ``crates/fno-agents/src/claims.rs``. Renewal used to
    preserve the recorded pid, and that is what made SUSPECT mean two things: a
    respawned worker renewing under a new pid left a claim byte-identical to a
    dead worker's, so nothing on disk separated a live session from a corpse and
    every reader that must not steal from the first was forced to protect the
    second (x-05be).

    Returns None - meaning leave the anchor alone - in three cases, each for its
    own reason:

      * The recorded pid is still LIVE. There is nothing to repair, and
        rewriting it would let any process holding the same holder string take
        over a running session's anchor.
      * The claim is off-machine. We cannot read another box's pid table, so a
        dead-looking pid there is unverified.
      * No harness ancestor resolves. There is no better anchor to write, and a
        transient pid is a worse one: ``fno-agents loop-check`` exits about a
        second after it renews, so anchoring to the renewer would re-file the
        corpse under a fresh number and fix nothing.

    PID-reuse detection survives because the anchor moves WITH the pid.
    ``_rebound_claim`` rewrites ``acquired_at`` alongside ``pid``, and the
    harness ancestor started before this renewal, so the claim reads live now
    and a later recycle of that pid number reads ``create_time > acquired_at``
    exactly as today.

    THE TRUST BOUNDARY, stated rather than implied. The renewer is authenticated
    by its holder string and nothing else, and `fno agents claim status` publishes that
    string. So a different session on this machine that refreshes under a
    published holder re-anchors the claim to ITS ancestor, and the claim then
    reads LIVE until that session ends instead of SUSPECT.

    That is the same credential `release_claim` and `refresh_claim` have always
    accepted, not a new one, and no stronger check is available here: the
    recorded pid is dead by precondition, so its ancestry cannot be walked to
    prove the renewer shares its session. Closing it needs a session identity in
    the claim record, which is its own change.
    """
    # An EXPIRED claim is already reclaimable, and re-anchoring one resurrects it
    # as LIVE - taking a slot a peer is entitled to and racing whatever recovery
    # was mid-flight. `renew_locked` in `crates/fno-agents/src/claims.rs`, which
    # this mirrors, has always refused there; without the same refusal here the
    # two implementations of one operation answered differently.
    if is_expired(existing):
        return None
    if is_live(existing) or not is_same_machine(existing.host, existing.machine_id):
        return None
    from .session_pid import resolve_session_pid

    anchor = resolve_session_pid(from_pid=os.getpid())
    if anchor is None:
        return None
    # ONLY when the move actually repairs the claim. `acquired_at` is held now
    # (the do row keys started_at on it), and `is_live` refuses a pid whose
    # create_time is AFTER it. A RESUMED session's harness process started after
    # the claim was filed, so anchoring to it would still classify SUSPECT while
    # overwriting the original holder's pid for nothing. Leave the anchor alone
    # there and let the TTL decide, which is what a claim with no better anchor
    # has always done.
    from .staleness import _process_create_time_ms

    created = _process_create_time_ms(anchor)
    if created is None or created > existing.acquired_at:
        return None
    return anchor


def refresh_claim(
    key: str,
    holder: str,
    *,
    ttl_ms: Optional[int] = None,
    root: Optional[Path] = None,
    _attempt: int = 0,
) -> Optional[Claim]:
    """Extend a TTL claim's expires_at.

    Returns the new Claim on success. Returns None for PID-liveness claims
    (no expires_at). An expired TTL claim raises :class:`ClaimValidationError`:
    it is reclaimable and must never be resurrected over concurrent recovery,
    and a distinct non-success keeps callers from misreporting it as the
    legitimate PID-liveness no-op.

    ``_attempt`` is internal bookkeeping only (never pass it): on mutex
    contention this recurses, bounded at ``ACQUIRE_MAX_ATTEMPTS`` (raises
    ``ClaimContended`` past that), mirroring ``acquire_claim``.

    Raises:
        HolderMismatch: existing claim is held by someone else.
        ClaimGoneAway: claim was released between read and rewrite.
        ClaimValidationError: claim expired before the locked rewrite.
        ClaimCorrupted: existing file fails parse/schema validation.
        ClaimContended: mutex contention exhausted ACQUIRE_MAX_ATTEMPTS retries.
    """
    if not key or not holder:
        raise ClaimValidationError("key and holder must be non-empty")
    if ttl_ms is not None and not (MIN_TTL_MS <= ttl_ms <= MAX_TTL_MS):
        raise ClaimValidationError(
            f"ttl_ms={ttl_ms} out of range [{MIN_TTL_MS}, {MAX_TTL_MS}]"
        )

    path = claim_path(key, root=root)
    if not path.exists():
        raise ClaimGoneAway(str(path))

    # Take the same per-key recovery mutex reap_dead_claims() holds while it
    # re-verifies and archives a claim it proved dead - unconditionally, not
    # just on contention, and read only once (under the lock). An unlocked
    # pre-read followed by a second locked re-read would parse the same YAML
    # file twice on every ordinary call; a single locked read costs one mkdir
    # (cheap, uncontended) instead. Without the lock, _atomic_replace happily
    # recreates `path` even if reap already archived it in the gap between a
    # read and this write - silently resurrecting a claim GC just removed.
    recovery_lock = path.with_name(path.name + RECOVERY_LOCK_SUFFIX)
    acquired_lock = False
    recovery_token = ""
    try:
        token = acquire_dir_mutex(
            recovery_lock, _RECOVERY_LOCK_MAX_WAIT_S, poll_s=_RECOVERY_LOCK_POLL_INTERVAL_S
        )
        if token is None:
            if _attempt + 1 >= ACQUIRE_MAX_ATTEMPTS:
                raise ClaimContended(
                    f"refresh_claim gave up after {ACQUIRE_MAX_ATTEMPTS} "
                    f"contention retries on {key!r}"
                )
            return refresh_claim(key, holder, ttl_ms=ttl_ms, root=root, _attempt=_attempt + 1)
        recovery_token = token
        acquired_lock = True

        existing = read_claim_file(path)
        if existing.holder != holder:
            raise HolderMismatch(expected=holder, actual=existing.holder, key=key)
        if existing.expires_at is None:
            return None
        if is_expired(existing):
            raise ClaimValidationError(
                f"claim {key!r} expired before refresh; refusing to resurrect it"
            )

        window = ttl_ms if ttl_ms is not None else MIN_TTL_MS
        anchor_pid = _reanchor_pid_for(existing)
        if anchor_pid is not None:
            refreshed = _rebound_claim(
                existing, anchor_pid, window, keep_acquired_at=True
            )
        else:
            refreshed = existing.model_copy(update={"expires_at": now_ms() + window})

        try:
            _atomic_replace(path, serialize_claim(refreshed))
        except FileNotFoundError as exc:
            # File was unlinked between our read and the rename.
            raise ClaimGoneAway(str(path)) from exc

        emit_claim_refreshed(refreshed, previous=existing)
        return refreshed
    finally:
        if acquired_lock:
            release_dir_mutex(recovery_lock, recovery_token)


def claim_status(key: str, *, root: Optional[Path] = None) -> dict[str, Any]:
    """Inspect a single key. Never raises; returns a structured dict.

    Keys in the returned dict:
        key:       echo of input
        state:     one of free | live | suspect | stale | corrupted
        holder:    string (only when state in {live, suspect, stale})
        pid, host, acquired_at, expires_at, reason, metadata: when readable
        error:     string (only when state == corrupted)
    """
    path = claim_path(key, root=root)
    try:
        claim = read_claim_file(path)
    except ClaimGoneAway:
        # Covers both "never existed" and "vanished before this read" - a
        # separate path.exists() pre-check would be a redundant stat, since
        # read_claim_file already turns a missing file into this same case.
        return {"key": key, "state": ClaimState.FREE.value}
    except ClaimCorrupted as exc:
        return {
            "key": key,
            "state": ClaimState.CORRUPTED.value,
            "error": str(exc),
            "path": str(path),
        }

    state = classify(claim)
    out: dict[str, Any] = {
        "key": key,
        "state": state.value,
        "holder": claim.holder,
        "pid": claim.pid,
        "host": claim.host,
        # Callers classify ownership from this dict, so it has to carry what
        # liveness actually compares; host alone sends them down the fallback.
        "machine_id": claim.machine_id,
        "acquired_at": claim.acquired_at,
        "expires_at": claim.expires_at,
    }
    if claim.reason is not None:
        out["reason"] = claim.reason
    if claim.harness is not None:
        out["harness"] = claim.harness
    if claim.metadata:
        out["metadata"] = claim.metadata
    return out


def _list_claims_impl(
    *,
    prefix: Optional[str] = None,
    include_stale: bool = False,
    root: Optional[Path] = None,
) -> tuple[list[dict[str, Any]], dict[str, int], dict[str, str]]:
    """Shared directory walk for ``list_claims`` and ``list_claims_with_counts``.

    Returns ``(rows, counts, states_by_key)``: ``rows`` is filtered per
    ``include_stale`` exactly as before; ``counts`` is every state seen
    while walking, regardless of what ``include_stale`` kept, so a caller
    can report what it withheld; ``states_by_key`` maps every key seen to
    its state (also independent of ``include_stale``) so a multi-root
    caller can dedup its own cross-root totals by key instead of summing
    ``counts`` blind to a key existing in more than one root.
    """
    cdir = claims_dir(root)
    # "free" covers a claim released between iterdir() and claim_status()
    # below (ClaimGoneAway / a vanished path) - without a bucket for it, that
    # entry silently drops out of `total` too, instead of being an accounted
    # non-event.
    counts = {"live": 0, "suspect": 0, "stale": 0, "corrupted": 0, "free": 0}
    if not cdir.is_dir():
        return [], {**counts, "total": 0}, {}

    out: list[dict[str, Any]] = []
    states_by_key: dict[str, str] = {}
    for entry in sorted(cdir.iterdir()):
        if entry.is_dir():
            # Skip the .expired archive dir and any future subdirs.
            continue
        if not entry.name.endswith(".lock"):
            continue

        key = decode_key(entry.name)
        if prefix is not None and not key.startswith(prefix):
            continue

        status = claim_status(key, root=root)
        state = status.get("state")
        if state in counts:
            counts[state] += 1
            states_by_key[key] = state
        # SUSPECT (x-ba4b) is an active, TTL-protected claim - it must count
        # alongside LIVE so lane accounting (advance._live_lane_domains) does not
        # under-count a slot held by a respawned worker and over-dispatch.
        if state in {ClaimState.LIVE.value, ClaimState.SUSPECT.value}:
            out.append(status)
        elif include_stale and state in {
            ClaimState.STALE.value,
            ClaimState.CORRUPTED.value,
        }:
            out.append(status)

    return out, {**counts, "total": sum(counts.values())}, states_by_key


def list_claims(
    *,
    prefix: Optional[str] = None,
    include_stale: bool = False,
    root: Optional[Path] = None,
) -> list[dict[str, Any]]:
    """Enumerate claims under the claims directory.

    Filters:
        prefix:        only return claims whose key starts with this string.
        include_stale: include stale + corrupted entries (default: live only).

    Corrupted entries are returned with state="corrupted" and an "error"
    key when ``include_stale=True``; they are skipped silently otherwise.
    """
    rows, _counts, _states = _list_claims_impl(
        prefix=prefix, include_stale=include_stale, root=root
    )
    return rows


def list_claims_with_counts(
    *,
    prefix: Optional[str] = None,
    include_stale: bool = False,
    root: Optional[Path] = None,
) -> tuple[list[dict[str, Any]], dict[str, int], dict[str, str]]:
    """Like :func:`list_claims`, but also returns the filtered-state counts.

    A store that is 99 percent stale must not render as an
    empty store. ``counts`` carries ``live``/``suspect``/``stale``/
    ``corrupted``/``total`` for every lockfile the walk saw, independent of
    what ``include_stale`` kept in ``rows`` - this is what lets a caller
    print what it filtered instead of the bare string ``no claims``. The
    third element, ``states_by_key``, is the same information keyed by
    claim key so a multi-root caller can dedup a key seen in more than one
    root before summing.
    """
    return _list_claims_impl(prefix=prefix, include_stale=include_stale, root=root)


def force_release_claim(
    key: str,
    reason: str,
    *,
    root: Optional[Path] = None,
    holding_recovery_lock: bool = False,
) -> None:
    """Administratively drop a claim, regardless of holder.

    ``reason`` is required (non-empty); the audit event records who ran the
    override and why. Idempotent: missing claim file is success. Existing
    claims are archived to ``.expired/`` rather than unlinked, so a forensic
    trail survives.

    ``holding_recovery_lock`` is for the one caller that already holds this
    key's recovery mutex and is calling from inside it. The mutex is a mkdir
    lock and mkdir locks are not reentrant, so without this the nested acquire
    below cannot ever succeed: it waits out the full timeout and then archives
    UNLOCKED, which is the exact race the lock exists to close, bought at five
    seconds per recovery.
    """
    if not key:
        raise ClaimValidationError("key must be non-empty")
    if not reason:
        raise ClaimValidationError("reason must be non-empty for force-release")

    path = claim_path(key, root=root)

    # Take the same per-key recovery mutex acquire_claim/refresh_claim/
    # reap_dead_claims all take. Without it, a concurrent idempotent
    # re-acquire or refresh can read the still-present claim under ITS OWN
    # lock and then write a fresh copy back via _atomic_replace's
    # unconditional rename right after this call's archive_claim moves the
    # file away - silently resurrecting the very claim this override just
    # removed (the same class of race the other verbs close, just left open
    # here). A contended mutex is a bounded wait like every other verb, never
    # a refusal: on timeout, force-release proceeds without it rather than
    # raising, preserving its "always succeeds" administrative-override
    # contract - the exposure narrows from "always racy" to "racy only past
    # a 5s timeout under sustained contention" instead of closing to zero.
    recovery_lock = path.with_name(path.name + RECOVERY_LOCK_SUFFIX)
    acquired_lock = False
    recovery_token = ""
    try:
        token = (
            None
            if holding_recovery_lock
            else acquire_dir_mutex(
                recovery_lock,
                _RECOVERY_LOCK_MAX_WAIT_S,
                poll_s=_RECOVERY_LOCK_POLL_INTERVAL_S,
            )
        )
        if token is not None:
            recovery_token = token
            acquired_lock = True

        if not path.exists():
            emit_claim_force_overridden(
                key=key, reason=reason, previous_holder=None, previous_pid=None,
            )
            return

        previous: Optional[Claim] = None
        try:
            previous = read_claim_file(path)
        except (ClaimCorrupted, ClaimGoneAway):
            previous = None

        archive_claim(path, ts_ms=now_ms())
        emit_claim_force_overridden(
            key=key,
            reason=reason,
            previous_holder=previous.holder if previous is not None else None,
            previous_pid=previous.pid if previous is not None else None,
        )
    finally:
        if acquired_lock:
            release_dir_mutex(recovery_lock, recovery_token)


def sweep_verdict(
    claim: Claim,
    *,
    now: Optional[int] = None,
    abandonment_probe: Optional[Callable[[Claim], Optional[bool]]] = None,
    node_settlement: Optional[Callable[..., Optional[bool]]] = None,
) -> tuple[bool, str]:
    """Can this claim be archived, and if not, which bucket says why?

    ``node_settlement`` runs FIRST on a ``node:`` claim and answers the
    question a pid cannot: is the claim's own node still the holder's
    workplace? A node that closed under the claim, or a holder whose roster
    row resolves to a DIFFERENT node, is positive evidence of abandonment -
    measured on the live 2026-08-21 specimen where a session that finished
    one node and moved to the next kept a LIVE claim on the dead one for 16
    hours, because "holder alive" was the only question asked (x-94f8).
    ``True`` settles (reapable); anything else falls through to liveness,
    which stays the authority for every unsettled shape.

    :func:`fno.claims.staleness.classify_for_sweep` plus the roster probe on the
    one case a pid cannot settle: a ``node:`` claim reading SUSPECT, whose
    holder is a session and so genuinely might come back under a new pid.

    THE single reap decision. The sweep's lock-free triage, its under-mutex
    re-verify, and the spawn guard's targeted recovery all call this, so no two
    of them can drift into different answers about the same claim - the shape
    the first pitfalls entry names.

    Buckets: ``""`` (reapable), ``"live"``, ``"offhost"``, ``"suspect"`` (no
    probe was supplied), ``"suspect_alive"`` (a worker is on the node) and
    ``"suspect_unprobed"`` (the probe could not run). The last two are kept
    apart deliberately: one is a measurement and the other is its absence.
    """
    if node_settlement is not None and claim.key.startswith("node:"):
        # A settlement instrument that raises answers nothing; a broken probe
        # must never become a verdict. The CLI-built settlement never raises
        # by contract - this is the belt under it.
        try:
            if node_settlement(claim, now=now) is True:
                return True, ""
        except Exception:  # noqa: BLE001 - unknown keeps
            pass
    provably_dead, bucket = classify_for_sweep(claim, now)
    if provably_dead or bucket != "suspect":
        return provably_dead, bucket
    if abandonment_probe is None or not claim.key.startswith("node:"):
        return False, "suspect"
    verdict = abandonment_probe(claim)
    if verdict is True:
        return True, ""
    # False: a live worker holds this node. None: the probe could not run,
    # which is unknown, and unknown keeps.
    return False, "suspect_alive" if verdict is False else "suspect_unprobed"


def _default_reap_roots() -> list[Path]:
    """Both claims roots swept by a bare ``fno agents claim reap`` (AC2).

    Global node claims live at ``claims_dir(global_claims_root())``
    (``~/.fno/claims`` by default); a repo's own root is
    ``claims_dir(None)`` (canonical repo root). A cwd whose canonical repo
    root IS the global root sweeps once, not twice - see
    :func:`fno.claims.io.dedup_claims_roots`.
    """
    return _dedup_roots([global_claims_root(), None])


def reap_dead_claims(
    *,
    roots: Optional[list[Optional[Path]]] = None,
    apply: bool = False,
    abandonment_probe: Optional[Callable[[Claim], Optional[bool]]] = None,
    node_settlement: Optional[Callable[..., Optional[bool]]] = None,
) -> dict[str, Any]:
    """Archive every provably-dead claim across one or more claims roots.

    The only mutation missing from the claim lifecycle. Acquire,
    release, refresh, and force-release all exist; nothing prunes a claim
    whose holder died without releasing, so a dead session leaks its
    lockfile forever. This walks every ``.lock`` file in the swept roots,
    classifies it with :func:`fno.claims.staleness.classify_for_sweep` (the
    single liveness authority; :func:`~fno.claims.staleness.is_provably_dead`
    is its bool-only view), and archives the provably-dead ones to
    ``.expired/``.

    ``roots``, when given, is a list of repo-root arguments passed through
    to :func:`fno.claims.io.claims_dir` exactly as ``--root`` does for
    every other claim verb (``None`` means the canonical repo root). When
    omitted, both default roots are swept in one run (AC2) - sweeping only
    one is the guard-on-one-of-N-paths trap: 574 of the claims measured on
    2026-08-14 lived in the root a single-root sweep would have missed.

    With ``apply=False`` (the default), nothing is written; reapable files
    are counted under ``would_reap`` from the same lock-free classification
    ``apply=True`` uses before it ever takes the per-key recovery mutex - a
    dry run never probes that mutex (deliberately: it is cheap, lock-free
    triage by design), so a claim it counts under ``would_reap`` can still
    land under ``contended`` in a LATER real apply run if something else
    holds that key's mutex at that later instant. That gap is no different
    from any other race between a preview and a separate later action; it is
    not a promise this call predicts contention outcomes, only that the
    classification itself (dead vs. live vs. suspect) matches.

    With ``apply=True``, each reapable file is archived and then the store
    is RE-READ to confirm the move: the source path must be gone and the
    ``.expired/`` destination must exist. Only that re-read increments
    ``reaped`` - never the absence of an exception, because ``fno agents
    rm`` was observed tonight to exit 0 having moved nothing. A file whose
    source path is still present after the archive call is counted under
    ``reap_failed`` with its path, and the caller (the ``reap`` CLI verb)
    exits non-zero when that list is non-empty.

    ``abandonment_probe`` is the SECOND instrument, and it is optional so that
    omitting it is byte-for-byte today's behavior. A ``node:`` claim reading
    SUSPECT (dead pid, unexpired TTL) is the one case a pid cannot settle: the
    holder is a session, and a session can be respawned under a new pid. The
    probe answers "is a live worker actually on this node" from the roster, and
    is called ONLY for a ``node:`` key that classified SUSPECT - never to
    override a live claim, and never for a key family with no roster to consult.

    Its three answers are deliberately not a bool:

      ``True``  proven abandoned; reap it.
      ``False`` a live worker is on the node; keep it (``kept_suspect_alive``).
      ``None``  the probe could not run; keep it (``kept_suspect_unprobed``).

    ``None`` KEEPS. Reaping because a probe returned nothing is the exact
    inversion of this fix: an instrument that did not run must never be read as
    a finding, and archiving a live worker's claim is x-ba4b's disaster from the
    other side.

    Returns a summary dict: ``scanned``, ``reaped``, ``would_reap``,
    ``kept_live``, ``kept_suspect``, ``kept_suspect_alive``,
    ``kept_suspect_unprobed``, ``kept_offhost``, ``corrupted``, ``vanished``,
    ``contended``, ``reap_failed`` (list of ``(path, reason)``), ``apply``,
    ``roots``. The two new suspect buckets split what used to be one number:
    "kept: 2 suspect" is the line that taught the operator the reaper was
    useless, because it could not say whether those two were protected or
    merely unmeasured. A ``claim_reap_swept`` event fires on every
    ``apply=True`` call, including a zero-reap run - a leg that never ran
    must not look the same as one that ran and found nothing. A dry run
    fires no event: the "nothing is written" promise above covers the
    event log too, so `fno backlog reconcile --dry-run`'s own preview
    contract is not silently broken by the reap it previews.
    """
    use_dirs = _default_reap_roots() if roots is None else _dedup_roots(roots)

    ts = now_ms()
    scanned = 0
    reaped = 0
    would_reap = 0
    kept: dict[str, int] = {
        "offhost": 0, "suspect": 0, "live": 0,
        "suspect_alive": 0, "suspect_unprobed": 0,
    }

    def _sweep_verdict(claim: Claim) -> tuple[bool, str]:
        return sweep_verdict(
            claim,
            now=ts,
            abandonment_probe=abandonment_probe,
            node_settlement=node_settlement,
        )

    corrupted = 0
    vanished = 0
    # A provably-dead claim whose recovery mutex is held by a genuine live
    # recovery (acquire_dir_mutex returned None on real contention, not a
    # stealable corpse) - the file is still on disk and still dead, just
    # left for the next sweep, so it must not be counted as vanished (which
    # means "gone from the store").
    contended = 0
    reap_failed: list[tuple[str, str]] = []
    # Node ids whose claims this run archived (confirmed re-reads only), for
    # the lock-mirror clear after the loop.
    settled_nodes: list[str] = []

    def _read_or_bucket(entry: Path) -> Optional[Claim]:
        """Read one claim file for the sweep, or bucket why it can't be read.

        Shared by the outer scan and the mutex re-verify below so the
        corrupted/vanished handling exists once, not twice.
        """
        nonlocal corrupted, vanished
        try:
            return read_claim_file(entry)
        except ClaimCorrupted:
            # Cannot classify what cannot be parsed; a claim that cannot be
            # read cannot be proven dead (AC6).
            corrupted += 1
            return None
        except ClaimGoneAway:
            # A concurrent release between listdir and read is a normal
            # outcome, not a failure of this run.
            vanished += 1
            return None

    for cdir in use_dirs:
        if not cdir.is_dir():
            continue
        root_label = str(cdir)
        for entry in sorted(cdir.iterdir()):
            if entry.is_dir():
                # Skip .expired/ and any future subdir.
                continue
            if not entry.name.endswith(".lock"):
                continue

            scanned += 1
            claim = _read_or_bucket(entry)
            if claim is None:
                continue

            # Cheap, lock-free triage: decide whether this claim is even a
            # reap candidate before paying for a mutex acquire. The mutex
            # re-verify below does NOT reuse this result - it re-reads the
            # file and calls classify_for_sweep again on that fresh read,
            # because the claim may have been archived-and-recreated between
            # this scan and the lock. Do not "de-duplicate" that second call;
            # it is the TOCTOU check, not redundant work.
            provably_dead, bucket = _sweep_verdict(claim)

            if provably_dead:
                if not apply:
                    would_reap += 1
                    continue

                # Take the same per-key recovery mutex acquire_claim() uses for
                # its own archive-then-recreate (core.py ~267-371) so this
                # cannot archive a claim a concurrent legitimate acquirer just
                # recreated at this path. timeout_s=0: try once, steal a
                # corpse left by a crashed reap/acquire if the dir is stale,
                # else give up immediately rather than wait - reap runs on a
                # cadence and blocking here would stall the whole sweep; a
                # live owner (no steal) means a real recovery is in flight,
                # left for the next sweep.
                recovery_lock = entry.with_name(entry.name + RECOVERY_LOCK_SUFFIX)
                recovery_token = acquire_dir_mutex(recovery_lock, 0)
                if recovery_token is None:
                    # A live, in-age holder - genuine contention, not a
                    # corpse (mutex.py's own contract). The file is still on
                    # disk and still provably dead; "vanished" would wrongly
                    # tell the operator it is gone from the store.
                    contended += 1
                    continue

                try:
                    # Re-read and re-verify under the mutex: the file may
                    # have been archived-and-recreated by acquire_claim()
                    # between our scan and this lock.
                    fresh = _read_or_bucket(entry)
                    if fresh is None:
                        continue

                    fresh_dead, fresh_bucket = _sweep_verdict(fresh)
                    if not fresh_dead:
                        kept[fresh_bucket] += 1
                        continue

                    try:
                        archive_path = archive_claim(entry, ts_ms=ts)
                    except OSError as exc:
                        # A permission error, full disk, or other rename
                        # failure must not abort the whole sweep - every
                        # other claim in this and later roots is still
                        # reapable. Bucket this one and keep scanning,
                        # matching the "move didn't happen" case below.
                        reap_failed.append((str(entry), f"archive_claim raised: {exc}"))
                        continue

                    if archive_path != entry and archive_path.exists():
                        # archive_path != entry proves archive_claim actually
                        # computed a distinct .expired/ destination (the
                        # idempotent-already-gone case returns entry itself
                        # unchanged); combined with .exists() on that unique,
                        # ts-suffixed path, that is durable proof THIS call's
                        # rename succeeded - regardless of what entry.exists()
                        # says afterward. A fresh, unrelated acquire_claim()
                        # can legitimately recreate a live claim at the same
                        # key the instant after this archive completes (its
                        # top-level atomic_create_exclusive never consults
                        # this recovery mutex once the path is empty), which
                        # would make entry.exists() true again with no
                        # bearing on whether the archive itself worked.
                        reaped += 1
                        emit_claim_reaped(
                            fresh,
                            root=root_label,
                            age_ms=max(0, ts - fresh.acquired_at),
                        )
                        if fresh.key.startswith("node:"):
                            settled_nodes.append(fresh.key[len("node:") :])
                    elif not entry.exists():
                        # archive_claim's idempotent short-circuit: the
                        # source was already fully cleared (a concurrent
                        # reap, or the holder's own delayed release) before
                        # this call, so it returned the source path itself,
                        # which does not exist either. The store no longer
                        # holds it, which is the outcome we wanted; we just
                        # cannot claim credit for the move.
                        vanished += 1
                    else:
                        # The positive-marker rule: an exit without exception
                        # is not evidence. The source is still there and no
                        # archive was created, so the move did not happen
                        # (AC5).
                        reap_failed.append(
                            (str(entry), "archive_claim did not move the file")
                        )
                finally:
                    release_dir_mutex(recovery_lock, recovery_token)
                continue

            # Not provably dead. Bucket the reason for the report.
            kept[bucket] += 1

    # The graph lock mirror for reaped node claims, cleared OUTSIDE the
    # per-key recovery mutex (after the sweep loop) so the process's only
    # lock ordering stays graph-then-claims (x-94f8). Without this, a reaped
    # worker's node keeps `locked_by` until LOCK_TTL_HOURS staleness clears
    # it lazily, reading `claimed` - held out of dispatch - for hours after
    # the reap. Dry runs never write, so they never reach this either.
    # Default sweeps only: an explicit --root sweep reads SOMEONE ELSE'S
    # claims tree, and this process's graph has no ownership relationship to
    # those node ids.
    lock_mirror_cleared = 0
    if apply and settled_nodes and roots is None:
        lock_mirror_cleared = _clear_lock_mirror_for_reaped(settled_nodes)

    summary: dict[str, Any] = {
        "scanned": scanned,
        "reaped": reaped,
        "would_reap": would_reap,
        "kept_live": kept["live"],
        "kept_suspect": kept["suspect"],
        "kept_suspect_alive": kept["suspect_alive"],
        "kept_suspect_unprobed": kept["suspect_unprobed"],
        "kept_offhost": kept["offhost"],
        "corrupted": corrupted,
        "vanished": vanished,
        "contended": contended,
        "reap_failed": reap_failed,
        "apply": apply,
        "lock_mirror_cleared": lock_mirror_cleared,
        "roots": [str(d) for d in use_dirs],
    }
    if apply:
        emit_claim_reap_swept(summary)
    return summary


def _clear_lock_mirror_for_reaped(node_ids: list[str]) -> int:
    """Clear ``locked_by``/``claimed_at`` on nodes whose claims were reaped.

    Best-effort: a graph failure is a named stderr line and never fails the
    sweep - the claim file is already gone, which is the load-bearing half.
    Unconditional across done nodes too: a node closed before the closure
    hook shipped keeps a mirror nothing else ever clears (statuses.py only
    clears stale locks on non-terminal rungs). A node whose claim file is
    BACK is skipped: a dispatcher can re-acquire in the window between this
    sweep's archive and the clear, and wiping that fresh lock is the
    second-worker disaster the whole reap doctrine exists to prevent.
    Returns how many entries were touched.
    """
    import sys

    from fno.claims.io import claims_root_for
    from fno.graph.store import locked_mutate_graph, read_graph
    from fno.paths import graph_json
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        # The locked_by mirror is graph-store state; under an external
        # tracker backend there is no graph mirror to clear.
        return 0

    wanted = set(node_ids)
    cleared: list[str] = []

    # Read first, mutate only if a reaped node is actually in the graph: a
    # sweep whose reaped ids match no graph row (tests, foreign repos) must
    # not take the graph lock and rewrite a file it has no change for.
    try:
        present = {
            e.get("id")
            for e in read_graph(graph_json())
            if isinstance(e, dict) and e.get("id") in wanted
        }
    except Exception as exc:  # noqa: BLE001 - mirror hygiene never fails the sweep
        print(f"claim reap: lock-mirror read failed: {exc}", file=sys.stderr)
        return 0
    if not present:
        return 0

    def _clear(entries: list[dict]) -> list[dict]:
        from fno.claims.core import claim_path
        from fno.claims.io import dedup_claims_roots

        for e in entries:
            if not (isinstance(e, dict) and e.get("id") in wanted):
                continue
            key = f"node:{e['id']}"
            if any(
                claim_path(key, root=r).exists()
                for r, _d in dedup_claims_roots([claims_root_for(key), None])
            ):
                continue  # re-acquired between archive and this clear
            e["locked_by"] = None
            e["claimed_at"] = None
            cleared.append(str(e.get("id")))
        return entries

    try:
        locked_mutate_graph(graph_json(), _clear)
    except Exception as exc:  # noqa: BLE001 - mirror hygiene never fails the sweep
        print(f"claim reap: lock-mirror clear failed: {exc}", file=sys.stderr)
    return len(cleared)


def _dedup_roots(roots: list[Optional[Path]]) -> list[Path]:
    """Resolve + dedup an explicit ``--root`` list, returning the claims dirs."""
    return [cdir for _, cdir in dedup_claims_roots(roots)]
