"""Age-based rename-steal + owner-token verification for the cross-language mkdir mutexes.

A ``mkdir``-style mutex is atomic to acquire but a killed holder leaves a dir
byte-identical to one a live worker is inside. On 2026-07-13 that starved every
``events.jsonl`` append for 8 days and made one claim key permanently
unrecoverable, so the lock carries an age signal: the dir's own mtime IS its
acquisition timestamp, and a dir older than ``STALE_MUTEX_STEAL_S`` is a corpse
any contender may rename-steal.

Stealing is unilateral, so a holder suspended past the threshold (laptop sleep,
SIGSTOP) can resume inside its critical section while a stealer is also in one.
Without an ownership check the resumed holder's trailing ``rmdir`` then deletes
whatever now sits at the path - the stealer's LIVE lock - and under full-suite
load those wrongful deletes cascaded past the fixed 2s append deadline (the
flaky ``concurrent_stealers_have_exactly_one_rename_winner`` and
``test_AC3_FR_concurrent_stealers_both_land``). The owner token closes that hole:
acquire stamps ``{host}:{pid}:{monotonic_ns}`` into ``lock_dir/owner`` and
release removes the dir only when the token matches, so a stealer's fresh lock
(token differs) survives the resumed holder's release.

Mirrored in ``crates/fno-agents/src/claims.rs`` (``STALE_MUTEX_STEAL`` /
``steal_if_stale`` / ``acquire_dir_mutex`` / ``release_dir_mutex``); the
``.recovery.d`` mutex is wire protocol between the two, so the threshold, the
steal rule, and the token format must change in lockstep.

Only steal a mutex whose critical section tolerates a concurrent holder: a
whole-line O_APPEND write does (two appends interleave harmlessly), which is why
the events mutex is stealable. A read-modify-write does NOT: the resumed holder
writes back a snapshot predating the stealer's update and silently loses it.
"""

from __future__ import annotations

import logging
import os
import shutil
import socket
import time
from pathlib import Path

log = logging.getLogger(__name__)

# Every critical section under these mutexes is sub-second (append one line;
# rename one file and exclusive-create another), so 120s is ~100x the honest
# hold time. Never do slow work (network, subprocess) inside one of these locks
# or this threshold stops being a corpse detector.
STALE_MUTEX_STEAL_S = 120

_HOST = socket.gethostname()


def _owner_token() -> str:
    """Unique-per-acquire token: ``host:pid:monotonic_ns``.

    Two acquires in one process differ on monotonic_ns; two processes differ on
    pid (or host). Unique by construction, so a token match is proof of identity
    and a mismatch is proof a stealer swapped the dir.
    """
    return f"{_HOST}:{os.getpid()}:{time.monotonic_ns()}"


def _read_owner(lock_dir: Path) -> str:
    try:
        return (lock_dir / "owner").read_text()
    except OSError:
        return ""


def _stamp_owner(lock_dir: Path) -> str:
    """Generate a token, write it to ``lock_dir/owner``, return the token.

    Called right after the mkdir acquire. The mkdir-to-stamp gap is safe by the
    age gate: a stealer only fires on dirs older than the threshold, so a fresh
    dir is never stolen mid-stamp. A crash between mkdir and stamp leaves a
    no-owner corpse the age gate steals exactly as before.
    """
    token = _owner_token()
    try:
        (lock_dir / "owner").write_text(token)
    except OSError:
        pass  # unwritable is exotic; the mkdir is the real acquire
    return token


def acquire_dir_mutex(
    lock_dir: Path, timeout_s: float, *, steal: bool = True, poll_s: float = 0.1
) -> str | None:
    """Acquire a mkdir dir mutex; return an owner token, or None on timeout.

    ``mkdir`` is atomic; on contention, steal a stale corpse (age-gated rename)
    or poll until ``timeout_s``. The returned token is also written to
    ``lock_dir/owner`` so the matching :func:`release_dir_mutex` can verify
    ownership before removing the dir. None means a live, in-age holder was held
    past the deadline - genuine congestion, not a corpse.
    """
    deadline = time.monotonic() + timeout_s
    while True:
        try:
            lock_dir.mkdir(parents=True)
        except FileExistsError:
            if steal and steal_if_stale(lock_dir):
                continue
            if time.monotonic() >= deadline:
                return None
            time.sleep(poll_s)
            continue
        return _stamp_owner(lock_dir)


def release_dir_mutex(lock_dir: Path, token: str) -> None:
    """Remove ``lock_dir`` only when its owner token matches; never raise.

    A mismatch (or a missing/unreadable owner file) means the lock was stolen or
    replaced out from under this holder mid-write: log once and leave the current
    holder's dir intact. This is the wrongful-delete vector the owner token
    exists to close. The dir now contains an ``owner`` file, so removal is
    ``rmtree``; a stolen-then-reaped dir is already gone and rmtree is a no-op.
    """
    if _read_owner(lock_dir) == token:
        shutil.rmtree(lock_dir, ignore_errors=True)
        return
    log.warning(
        "release_dir_mutex: %s no longer owned by %s; left intact (stolen or replaced)",
        lock_dir,
        token,
    )


def steal_if_stale(lock_dir: Path) -> bool:
    """Rename-steal ``lock_dir`` when it is older than ``STALE_MUTEX_STEAL_S``.

    Returns True when the caller should retry its ``mkdir`` immediately: either
    the corpse was stolen, or the lock was already gone. False means the lock is
    honestly held (or a live lock was swapped in after we aged the path) and the
    caller should wait exactly as it did before.
    """
    # lstat, not stat: a dangling symlink at the lock path is stattable only
    # without following it, and `mkdir` reports it as EEXIST. Following would
    # raise ENOENT here, and a caller that retries on True would then spin
    # against a lock it can never acquire.
    try:
        before = lock_dir.lstat()
    except FileNotFoundError:
        return True  # released between contention and stat: retry the mkdir
    except OSError:
        return False  # unstattable for any other reason: wait, never spin

    age = time.time() - before.st_mtime
    if age <= STALE_MUTEX_STEAL_S:
        return False

    # Capture the corpse's identity BEFORE the rename: between this and the
    # rename another stealer can win and a fresh holder acquire at the same
    # path, so what we move may be a LIVE lock, not the corpse we aged. The
    # owner token is the identity check (inode recycling fooled the old
    # inode+mtime compare).
    before_token = _read_owner(lock_dir)

    # Unique per attempt: reusing one name per pid means a reap dir left behind
    # by a failed cleanup collides forever, silently disabling every future
    # steal by this process.
    reaped = lock_dir.with_name(f"{lock_dir.name}.reap.{os.getpid()}.{time.monotonic_ns()}")
    try:
        os.rename(lock_dir, reaped)
    except FileNotFoundError:
        return True  # released while we were looking
    except OSError as exc:
        # Usually another stealer won the rename. Anything else (EACCES, EXDEV)
        # would otherwise disable stealing silently, so say which lock and why.
        log.warning("could not steal stale mutex %s: %s", lock_dir, exc)
        return False

    if not _same_owner(reaped, before_token):
        # A live lock was swapped in between the age check and the rename: put
        # it back and lose the race properly.
        try:
            os.rename(reaped, lock_dir)
        except OSError:
            log.warning("stole a live mutex at %s and could not restore it", lock_dir)
        return False

    log.warning("stole stale mutex %s (age %ds) -> %s", lock_dir, int(age), reaped)
    _remove(reaped)
    return True


def _same_owner(path: Path, before_token: str) -> bool:
    """Identity for a reaped lock dir via owner token.

    A token match means we reaped what we aged. An empty owner file means a
    pre-token corpse (a crashed acquirer that died before stamping, or an old
    binary with no token) - reaped as today. Any other token means a live lock
    was swapped in, so the caller puts it back.
    """
    after = _read_owner(path)
    return after == "" or after == before_token


def _remove(path: Path) -> None:
    """Delete a reaped mutex (a dir with an owner file, or a symlink)."""
    try:
        os.unlink(path)  # rmtree raises NotADirectoryError on a symlink
    except OSError:
        shutil.rmtree(path, ignore_errors=True)
