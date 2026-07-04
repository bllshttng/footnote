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
    emit_claim_refreshed,
    emit_claim_released,
    emit_claim_stale_reclaimed,
)
from .io import (
    ClaimAlreadyHeld,
    ClaimCorrupted,
    ClaimGoneAway,
    archive_claim,
    atomic_create_exclusive,
    claim_path,
    claims_dir,
    decode_key,
    read_claim_file,
    serialize_claim,
)
from .staleness import classify, now_ms
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


# Re-export low-level exceptions so callers can ``from fno.claims import ClaimGoneAway``.
__all__ = [
    "ClaimAlreadyHeld",
    "ClaimCorrupted",
    "ClaimGoneAway",
    "ClaimHeldByOther",
    "ClaimValidationError",
    "HolderMismatch",
    "acquire_claim",
    "claim_status",
    "force_release_claim",
    "list_claims",
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
) -> Claim:
    acquired = now_ms()
    return Claim(
        key=key,
        holder=holder,
        acquired_at=acquired,
        expires_at=(acquired + ttl_ms) if ttl_ms is not None else None,
        pid=pid if pid is not None else os.getpid(),
        host=host if host is not None else socket.gethostname(),
        reason=reason,
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

    new_claim = _make_claim(key, holder, ttl_ms, reason, metadata, pid, host)
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
            root=root,
        )

    if existing.holder == holder:
        # Idempotent re-acquire: refresh pid/host/acquired_at.
        refreshed = _make_claim(key, holder, ttl_ms, reason, metadata, pid, host)
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
        try:
            try:
                recovery_lock.mkdir(parents=True)
                acquired_lock = True
            except FileExistsError:
                # Another worker is doing recovery. Wait briefly, then recurse
                # from the top. The recovering worker will either succeed
                # (we then see live-other) or fail (we get another shot).
                _wait_for_recovery_release(recovery_lock)
                return acquire_claim(
                    key,
                    holder,
                    reason=reason,
                    ttl_ms=ttl_ms,
                    metadata=metadata,
                    pid=pid,
                    host=host,
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
                        root=root,
                    )
                emit_claim_acquired(new_claim)
                return new_claim

            if existing.holder == holder:
                # Raced into the idempotent path while we were grabbing the lock.
                refreshed = _make_claim(key, holder, ttl_ms, reason, metadata, pid, host)
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
            atomic_create_exclusive(path, payload)
            emit_claim_stale_reclaimed(new_claim, previous=existing)
            return new_claim
        finally:
            if acquired_lock:
                try:
                    recovery_lock.rmdir()
                except OSError:
                    pass

    # Live and not us => block.
    raise ClaimHeldByOther(
        holder=existing.holder,
        pid=existing.pid,
        host=existing.host,
        key=key,
    )


_RECOVERY_LOCK_POLL_INTERVAL_S = 0.02
_RECOVERY_LOCK_MAX_WAIT_S = 5.0


def _wait_for_recovery_release(recovery_lock: Path) -> None:
    """Poll briefly for a recovery lock to be released by another worker.

    The lock is just a directory; once the holder finishes, it rmdir()s and
    we can recurse. Bounded wait protects against a recovering worker that
    crashed and left a stale recovery lock (the next iteration through
    acquire_claim will then re-evaluate and possibly try recovery itself).
    """
    deadline = time.monotonic() + _RECOVERY_LOCK_MAX_WAIT_S
    while recovery_lock.exists() and time.monotonic() < deadline:
        time.sleep(_RECOVERY_LOCK_POLL_INTERVAL_S)
    # Deadline expired with the lock still held. Do NOT rmdir - the holder
    # may still be inside the critical section (a slow archive + create on
    # a heavily-loaded filesystem). Stealing the lockdir would cause two
    # workers to both run the archive+create sequence simultaneously,
    # reintroducing the TOCTOU double-winner bug the mutex is meant to
    # prevent. The waiter recurses into acquire_claim regardless; the
    # next attempt re-evaluates the claim and either sees the recovered
    # state or tries its own recovery. A truly stuck recovery is
    # recoverable via `fno claim force-release` from an operator.


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
) -> None:
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
    """
    if not key or not holder:
        raise ClaimValidationError("key and holder must be non-empty")

    path = claim_path(key, root=root)
    if not path.exists():
        return

    try:
        existing = read_claim_file(path)
    except ClaimGoneAway:
        return
    except ClaimCorrupted:
        # Corrupted file: we cannot verify ownership. Conservative default
        # is to leave it for force_release. strict mode surfaces the issue.
        if strict:
            raise
        return

    if existing.holder != holder:
        if strict:
            raise HolderMismatch(expected=holder, actual=existing.holder, key=key)
        return

    duration_ms = max(0, now_ms() - existing.acquired_at)
    try:
        path.unlink()
    except FileNotFoundError:
        return
    emit_claim_released(existing, duration_ms=duration_ms)


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
        "acquired_at": claim.acquired_at,
        "expires_at": claim.expires_at,
    }
    if claim.reason is not None:
        out["reason"] = claim.reason
    if claim.metadata:
        out["metadata"] = claim.metadata
    return out


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
    cdir = claims_dir(root)
    if not cdir.is_dir():
        return []

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

    return out


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
