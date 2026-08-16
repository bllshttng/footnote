"""High-level claim verbs.

Six operations on top of io + staleness:

    acquire_claim     - try to take a claim; idempotent re-acquire,
                        stale recovery, live-other detection.
    release_claim     - drop a claim we own.
    refresh_claim     - extend TTL on a claim we own (no-op for PID-liveness).
    claim_status      - inspect a single key.
    list_claims       - enumerate all live (and optionally stale) claims.
    force_release_claim - administrative override, always succeeds.

Every state-changing verb appends an audit event to ``.fno/events.jsonl``
through the typed builders in :mod:`fno.claims.events`. Audit-trail
writes are best-effort: the YAML lock file write is authoritative.
"""
from __future__ import annotations

import os
import socket
import time
from pathlib import Path
from typing import Any, Optional

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
from .staleness import classify, now_ms
from ..harness_identity import resolve_harness_identity
from ..mutex import _stamp_owner, acquire_dir_mutex, release_dir_mutex, steal_if_stale
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
    "ClaimAlreadyHeld",
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
        # (`fno claim` forwards to this Python CLI), kept in lockstep with the
        # Rust make_claim resolver via the shared harness_identity markers.
        # An explicit `harness` wins over ambient resolution so callers can pin
        # the owning harness deterministically.
        harness=harness if harness is not None else resolve_harness_identity().harness,
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
    """
    _validate_inputs(key, holder, ttl_ms)
    path = claim_path(key, root=root)

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
        )

    if existing.holder == holder:
        # Idempotent re-acquire: refresh pid/host/acquired_at.
        refreshed = _make_claim(key, holder, ttl_ms, reason, metadata, pid, host, harness)
        _atomic_replace(path, serialize_claim(refreshed))
        emit_claim_idempotent_reacquired(refreshed, previous=existing)
        return refreshed

    # Stale? Try recovery under a mkdir-based recovery mutex so the archive +
    # recreate steps are serialized across concurrent workers. Without the
    # mutex, two workers can both observe a stale file, both archive (one
    # actually moves, one no-ops), and both successfully create the new lock
    # in the gap between archive-and-create.
    if not _existing_is_live(existing):
        recovery_lock = path.with_name(path.name + ".recovery.d")
        acquired_lock = False
        recovery_token = ""
        try:
            try:
                recovery_lock.mkdir(parents=True)
                recovery_token = _stamp_owner(recovery_lock)
                acquired_lock = True
            except FileExistsError:
                # Another worker is doing recovery -- or died holding the mutex.
                # Steal a corpse (age-based, rename-atomic) so a killed
                # recoverer cannot brick this key forever; otherwise wait
                # briefly. Either way recurse from the top: the recovering
                # worker will either succeed (we then see live-other) or fail
                # (we get another shot).
                if not steal_if_stale(recovery_lock):
                    _wait_for_recovery_release(recovery_lock)
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
                )

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
                    )
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
                )
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


def _rebound_claim(existing: Claim, new_pid: int, ttl_ms: Optional[int]) -> Claim:
    """A rebound claim: identity fields preserved, process anchor + lease fresh.

    The native-resume rebind keeps ``key``/``holder``/``reason``/``metadata``/
    ``harness`` (one target attempt retains one symbolic owner) and rewrites
    only ``pid``/``host``/``machine_id``/``acquired_at``/``expires_at``. A
    PID-liveness claim stays PID-liveness (``ttl_ms`` None and no prior
    ``expires_at``); a TTL claim refreshes to ``now + prior_window`` so the
    deadline never compounds across rebinds (the renew() lesson).
    """
    acquired = now_ms()
    if ttl_ms is not None:
        expires_at: Optional[int] = acquired + ttl_ms
    elif existing.expires_at is not None:
        expires_at = acquired + max(existing.expires_at - existing.acquired_at, MIN_TTL_MS)
    else:
        expires_at = None
    return Claim(
        schema_version=existing.schema_version,
        key=existing.key,
        holder=existing.holder,
        acquired_at=acquired,
        expires_at=expires_at,
        pid=new_pid,
        host=socket.gethostname(),
        machine_id=machine_id() or None,
        reason=existing.reason,
        harness=existing.harness,
        metadata=existing.metadata,
    )


def compare_and_rebind(
    key: str,
    expected_holder: str,
    *,
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
    is the explicit ``fno target start`` successor path). Emits ``claim_rebound``;
    raises ``RebindRefused`` on any refusal.

    Returns ``(claim, mode)`` where mode is ``"rebind"`` (a dead prior PID was
    rebound) or ``"idempotent"`` (a live same-PID lease refresh).
    """
    _validate_inputs(key, expected_holder, ttl_ms)
    path = claim_path(key, root=root)
    npid = new_pid if new_pid is not None else os.getpid()
    recovery_lock = path.with_name(path.name + ".recovery.d")
    acquired_lock = False
    recovery_token = ""
    try:
        try:
            recovery_lock.mkdir(parents=True)
            recovery_token = _stamp_owner(recovery_lock)
            acquired_lock = True
        except FileExistsError:
            # A peer is mid-recovery, or a recoverer died holding the mutex.
            # Steal a corpse so a killed peer cannot brick the rebind; else wait.
            if steal_if_stale(recovery_lock):
                try:
                    recovery_lock.mkdir(parents=True)
                    recovery_token = _stamp_owner(recovery_lock)
                    acquired_lock = True
                except FileExistsError:
                    pass
            if not acquired_lock:
                _wait_for_recovery_release(recovery_lock)
                raise RebindRefused(
                    "claim recovery mutex busy; retry the resume bind",
                    state=None,
                )

        # Inside the mutex: re-read + classify before any mutation.
        try:
            existing = read_claim_file(path)
        except ClaimGoneAway:
            raise RebindRefused(
                "claim vanished; this target no longer owns the node "
                "(use `fno target start` to reclaim)",
                state="free",
            )
        except ClaimCorrupted:
            raise RebindRefused(
                "claim corrupted; cannot verify ownership (use `fno claim release --force`)",
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
        if state == ClaimState.LIVE:
            if existing.pid == npid:
                # Idempotent: already bound to this process; refresh lease only.
                rebound = _rebound_claim(existing, npid, ttl_ms)
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
        rebound = _rebound_claim(existing, npid, ttl_ms)
        _atomic_replace(path, serialize_claim(rebound))
        if emit:
            emit_claim_rebound(
                rebound,
                previous_pid=existing.pid,
                previous_state=state.value,
                mode="rebound",
                fno_id=fno_id,
                harness=harness_tag,
                harness_session_id=harness_session_id,
            )
        return rebound, "rebound"
    finally:
        if acquired_lock:
            release_dir_mutex(recovery_lock, recovery_token)


_RECOVERY_LOCK_POLL_INTERVAL_S = 0.02
_RECOVERY_LOCK_MAX_WAIT_S = 5.0


def _wait_for_recovery_release(recovery_lock: Path) -> None:
    """Poll briefly for a recovery lock to be released by another worker.

    The lock is just a directory; once the holder finishes, it rmdir()s and
    we can recurse. Only reached for a mutex young enough to be honestly held
    - a corpse is stolen by ``steal_if_stale`` before this is called.
    """
    # lexists, not exists: a dangling symlink at the mutex path is EEXIST to
    # mkdir but absent to a following stat, so exists() would report the lock
    # free and send the caller straight back into acquire_claim, recursing
    # without pause until RecursionError.
    deadline = time.monotonic() + _RECOVERY_LOCK_MAX_WAIT_S
    while os.path.lexists(recovery_lock) and time.monotonic() < deadline:
        time.sleep(_RECOVERY_LOCK_POLL_INTERVAL_S)
    # Deadline expired with the lock still held, and it is too young to be a
    # corpse. Do NOT rmdir - the holder may still be inside the critical
    # section (a slow archive + create on a heavily-loaded filesystem), and
    # removing it in place would let two workers run archive+create at once.
    # The waiter recurses into acquire_claim regardless; the next attempt
    # re-evaluates the claim and either sees the recovered state or tries its
    # own recovery, stealing the mutex once it ages past the threshold.


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


def refresh_claim(
    key: str,
    holder: str,
    *,
    ttl_ms: Optional[int] = None,
    root: Optional[Path] = None,
) -> Optional[Claim]:
    """Extend a TTL claim's expires_at.

    Returns the new Claim on success. Returns None for PID-liveness claims
    (no expires_at) - the call is a no-op by design; the caller is told
    via the return type rather than an exception so refresh is safe to
    call from a generic timer that does not know the claim's mode.

    Raises:
        HolderMismatch: existing claim is held by someone else.
        ClaimGoneAway: claim was released between read and rewrite.
        ClaimCorrupted: existing file fails parse/schema validation.
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

    existing = read_claim_file(path)
    if existing.holder != holder:
        raise HolderMismatch(expected=holder, actual=existing.holder, key=key)

    if existing.expires_at is None:
        return None

    new_expires = now_ms() + (ttl_ms if ttl_ms is not None else MIN_TTL_MS)
    refreshed = existing.model_copy(update={"expires_at": new_expires})

    try:
        _atomic_replace(path, serialize_claim(refreshed))
    except FileNotFoundError as exc:
        # File was unlinked between the existence check and the rename.
        raise ClaimGoneAway(str(path)) from exc

    emit_claim_refreshed(refreshed, previous=existing)
    return refreshed


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
    if not path.exists():
        return {"key": key, "state": ClaimState.FREE.value}

    try:
        claim = read_claim_file(path)
    except ClaimGoneAway:
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
) -> tuple[list[dict[str, Any]], dict[str, int]]:
    """Shared directory walk for ``list_claims`` and ``list_claims_with_counts``.

    Returns ``(rows, counts)``: ``rows`` is filtered per ``include_stale``
    exactly as before; ``counts`` is every state seen while walking,
    regardless of what ``include_stale`` kept, so a caller can report what
    it withheld (x-aeeb AC7).
    """
    cdir = claims_dir(root)
    # "free" covers a claim released between iterdir() and claim_status()
    # below (ClaimGoneAway / a vanished path) - without a bucket for it, that
    # entry silently drops out of `total` too, instead of being an accounted
    # non-event (x-aeeb review).
    counts = {"live": 0, "suspect": 0, "stale": 0, "corrupted": 0, "free": 0}
    if not cdir.is_dir():
        return [], {**counts, "total": 0}

    out: list[dict[str, Any]] = []
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

    return out, {**counts, "total": sum(counts.values())}


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
    rows, _counts = _list_claims_impl(prefix=prefix, include_stale=include_stale, root=root)
    return rows


def list_claims_with_counts(
    *,
    prefix: Optional[str] = None,
    include_stale: bool = False,
    root: Optional[Path] = None,
) -> tuple[list[dict[str, Any]], dict[str, int]]:
    """Like :func:`list_claims`, but also returns the filtered-state counts.

    x-aeeb AC7: a store that is 99 percent stale must not render as an
    empty store. ``counts`` carries ``live``/``suspect``/``stale``/
    ``corrupted``/``total`` for every lockfile the walk saw, independent of
    what ``include_stale`` kept in ``rows`` - this is what lets a caller
    print what it filtered instead of the bare string ``no claims``.
    """
    return _list_claims_impl(prefix=prefix, include_stale=include_stale, root=root)


def force_release_claim(
    key: str,
    reason: str,
    *,
    root: Optional[Path] = None,
) -> None:
    """Administratively drop a claim, regardless of holder.

    ``reason`` is required (non-empty); the audit event records who ran the
    override and why. Idempotent: missing claim file is success. Existing
    claims are archived to ``.expired/`` rather than unlinked, so a forensic
    trail survives.
    """
    if not key:
        raise ClaimValidationError("key must be non-empty")
    if not reason:
        raise ClaimValidationError("reason must be non-empty for force-release")

    path = claim_path(key, root=root)
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


def _default_reap_roots() -> list[Path]:
    """Both claims roots swept by a bare ``fno claim reap`` (AC2).

    Global node claims live at ``claims_dir(global_claims_root())``
    (``~/.fno/claims`` by default); a repo's own root is
    ``claims_dir(None)`` (canonical repo root). A cwd whose canonical repo
    root IS the global root sweeps once, not twice - see
    :func:`fno.claims.io.dedup_claims_roots`.
    """
    return _dedup_roots([global_claims_root(), None])


def _classify_for_sweep(claim: Claim, ts: int) -> tuple[bool, str]:
    """Classify one claim for ``reap_dead_claims``, mirroring
    :func:`fno.claims.staleness.is_provably_dead`'s own composition
    (same-machine and ``classify() is STALE``) so the single ``classify()``
    call is shared by both the outer scan and the mutex re-verify below
    instead of paid twice or three times per claim, and so a claim's
    classification stays consistent with the rest of one sweep pass by using
    the sweep's ``ts`` rather than a fresh clock read each time.

    Returns ``(provably_dead, bucket)`` where ``bucket`` is one of
    ``"offhost"``/``"suspect"``/``"live"`` - only meaningful when
    ``provably_dead`` is False.
    """
    if not is_same_machine(claim.host, claim.machine_id):
        return False, "offhost"
    state = classify(claim, now=ts)
    if state is ClaimState.STALE:
        return True, ""
    return False, "suspect" if state is ClaimState.SUSPECT else "live"


def reap_dead_claims(
    *,
    roots: Optional[list[Optional[Path]]] = None,
    apply: bool = False,
) -> dict[str, Any]:
    """Archive every provably-dead claim across one or more claims roots.

    x-aeeb: the only mutation missing from the claim lifecycle. Acquire,
    release, refresh, and force-release all exist; nothing prunes a claim
    whose holder died without releasing, so a dead session leaks its
    lockfile forever. This walks every ``.lock`` file in the swept roots,
    classifies it with :func:`_classify_for_sweep`, which mirrors
    :func:`fno.claims.staleness.is_provably_dead`'s three-part proof (the
    single liveness authority - see that function) but shares one
    ``classify()`` call across a claim's scan and re-verify instead of
    calling the bool-only predicate twice, and archives the provably-dead
    ones to ``.expired/``.

    ``roots``, when given, is a list of repo-root arguments passed through
    to :func:`fno.claims.io.claims_dir` exactly as ``--root`` does for
    every other claim verb (``None`` means the canonical repo root). When
    omitted, both default roots are swept in one run (AC2) - sweeping only
    one is the guard-on-one-of-N-paths trap: 574 of the claims measured on
    2026-08-14 lived in the root a single-root sweep would have missed.

    With ``apply=False`` (the default), nothing is written; reapable files
    are counted under ``would_reap`` so a dry run reports exactly what an
    apply run would do.

    With ``apply=True``, each reapable file is archived and then the store
    is RE-READ to confirm the move: the source path must be gone and the
    ``.expired/`` destination must exist. Only that re-read increments
    ``reaped`` - never the absence of an exception, because ``fno agents
    rm`` was observed tonight to exit 0 having moved nothing. A file whose
    source path is still present after the archive call is counted under
    ``reap_failed`` with its path, and the caller (the ``reap`` CLI verb)
    exits non-zero when that list is non-empty.

    Returns a summary dict: ``scanned``, ``reaped``, ``would_reap``,
    ``kept_live``, ``kept_suspect``, ``kept_offhost``, ``corrupted``,
    ``vanished``, ``reap_failed`` (list of ``(path, reason)``), ``apply``,
    ``roots``. A ``claim_reap_swept`` event fires on every call, apply or
    dry-run, including a zero-reap run - a leg that never ran must not
    look the same as one that ran and found nothing.
    """
    use_dirs = _default_reap_roots() if roots is None else _dedup_roots(roots)

    ts = now_ms()
    scanned = 0
    reaped = 0
    would_reap = 0
    kept_live = 0
    kept_suspect = 0
    kept_offhost = 0
    corrupted = 0
    vanished = 0
    reap_failed: list[tuple[str, str]] = []

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
            try:
                claim = read_claim_file(entry)
            except ClaimCorrupted:
                # Cannot classify what cannot be parsed; a claim that
                # cannot be read cannot be proven dead (AC6).
                corrupted += 1
                continue
            except ClaimGoneAway:
                # A concurrent release between listdir and read is a
                # normal outcome, not a failure of this run.
                vanished += 1
                continue

            # is_same_machine + classify() computed once per claim and reused
            # by both this outer scan and the mutex re-verify below, instead
            # of paid twice or three times per claim - the sweep runs on
            # every SessionStart/megawalk stop hook (x-aeeb review).
            provably_dead, bucket = _classify_for_sweep(claim, ts)

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
                # left for the next sweep (x-aeeb review: shares
                # fno.mutex.acquire_dir_mutex instead of hand-rolling the
                # same mkdir/steal/retry dance a third time).
                recovery_lock = entry.with_name(entry.name + ".recovery.d")
                recovery_token = acquire_dir_mutex(recovery_lock, 0)
                if recovery_token is None:
                    vanished += 1
                    continue

                try:
                    # Re-read and re-verify under the mutex: the file may
                    # have been archived-and-recreated by acquire_claim()
                    # between our scan and this lock.
                    try:
                        fresh = read_claim_file(entry)
                    except ClaimGoneAway:
                        vanished += 1
                        continue
                    except ClaimCorrupted:
                        corrupted += 1
                        continue

                    fresh_dead, fresh_bucket = _classify_for_sweep(fresh, ts)
                    if not fresh_dead:
                        if fresh_bucket == "offhost":
                            kept_offhost += 1
                        elif fresh_bucket == "suspect":
                            kept_suspect += 1
                        else:
                            kept_live += 1
                        continue

                    archive_path = archive_claim(entry, ts_ms=ts)
                    if not entry.exists() and archive_path.exists():
                        reaped += 1
                        emit_claim_reaped(
                            fresh,
                            root=root_label,
                            age_ms=max(0, ts - fresh.acquired_at),
                        )
                    elif not entry.exists() and not archive_path.exists():
                        # Some other actor (a concurrent reap, or the holder's
                        # own delayed release) fully cleared this file between
                        # our classification and our archive call. The store
                        # no longer holds it, which is the outcome we wanted;
                        # we just cannot claim credit for the move.
                        vanished += 1
                    else:
                        # The positive-marker rule: an exit without exception
                        # is not evidence. The source is still there, so the
                        # move did not happen (AC5).
                        reap_failed.append(
                            (str(entry), "archive_claim did not move the file")
                        )
                finally:
                    release_dir_mutex(recovery_lock, recovery_token)
                continue

            # Not provably dead. Bucket the reason for the report.
            if bucket == "offhost":
                kept_offhost += 1
            elif bucket == "suspect":
                kept_suspect += 1
            else:
                kept_live += 1

    summary: dict[str, Any] = {
        "scanned": scanned,
        "reaped": reaped,
        "would_reap": would_reap,
        "kept_live": kept_live,
        "kept_suspect": kept_suspect,
        "kept_offhost": kept_offhost,
        "corrupted": corrupted,
        "vanished": vanished,
        "reap_failed": reap_failed,
        "apply": apply,
        "roots": [str(d) for d in use_dirs],
    }
    emit_claim_reap_swept(summary)
    return summary


def _dedup_roots(roots: list[Optional[Path]]) -> list[Path]:
    """Resolve + dedup an explicit ``--root`` list, returning the claims dirs."""
    return [cdir for _, cdir in dedup_claims_roots(roots)]
