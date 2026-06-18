"""Mutual exclusion for concurrent ``fno review`` invocations.

Uses ``fcntl.flock(LOCK_EX | LOCK_NB)`` - cheap, POSIX-only, automatically
released by the kernel when the holding process dies (no stale-lock problem).

Windows is explicitly unsupported. The context manager raises
``NotImplementedError`` on ``sys.platform == "win32"`` rather than silently
doing nothing. A portable LockFile shim is out of scope for this phase.

Public API:
    ReviewLockBusy     - raised when the lock is held by another process
    acquire_review_lock - context manager; yields the lock file path
"""

from __future__ import annotations

import os
import sys
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

try:
    import fcntl as _fcntl
except ImportError:
    _fcntl = None  # type: ignore[assignment]


class ReviewLockBusy(RuntimeError):
    """Raised when another process holds the review lock for this session.

    Attributes:
        holder_pid: PID read from the lock file, or None if unparseable.
        lock_path:  Path to the lock file that was attempted.
    """

    def __init__(self, *, holder_pid: int | None, lock_path: Path) -> None:
        self.holder_pid = holder_pid
        self.lock_path = lock_path
        super().__init__(
            f"review lock held by pid {holder_pid} ({lock_path})"
        )


@contextmanager
def acquire_review_lock(
    session_id: str,
    *,
    artifacts_dir: Path,
) -> Iterator[Path]:
    """Acquire an exclusive non-blocking flock for ``session_id``.

    Opens ``<artifacts_dir>/review-<session_id>.lock`` (creating parent
    directories as needed), attempts ``LOCK_EX | LOCK_NB``.

    On ``BlockingIOError`` (another process holds the lock), reads the
    holder PID from the file and raises :exc:`ReviewLockBusy`.

    On success, writes ``os.getpid()`` to the file (truncate+write) and
    yields the lock-file path. The lock is released and the file is
    unlinked on context exit (normal or exceptional).

    Args:
        session_id:    Unique session identifier - scopes the lock.
        artifacts_dir: Directory in which to create the lock file.
                       Typically ``.fno/artifacts/`` for the repo.

    Yields:
        Path to the lock file (exists and is writable while held).

    Raises:
        ReviewLockBusy:      Another process holds the lock.
        NotImplementedError: Called on Windows (fcntl not available).
    """
    if sys.platform == "win32":
        raise NotImplementedError(
            "fcntl.flock not available on Windows; "
            "a portable lock shim is a follow-up task."
        )

    if _fcntl is None:
        raise NotImplementedError(
            "fcntl module not available on this platform; "
            "fcntl.flock mutual exclusion requires POSIX."
        )

    lock_path = artifacts_dir / f"review-{session_id}.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        try:
            _fcntl.flock(fd, _fcntl.LOCK_EX | _fcntl.LOCK_NB)
        except BlockingIOError:
            # Read the holder PID from the file before raising.
            holder_pid: int | None = None
            try:
                raw = os.pread(fd, 64, 0).decode(errors="replace").strip()
                if raw:
                    holder_pid = int(raw)
            except (OSError, ValueError):
                pass
            raise ReviewLockBusy(holder_pid=holder_pid, lock_path=lock_path)

        # Write our PID (truncate then write).
        pid_bytes = f"{os.getpid()}\n".encode()
        os.ftruncate(fd, 0)
        os.pwrite(fd, pid_bytes, 0)

        try:
            yield lock_path
        finally:
            # Release the flock and clean up.
            _fcntl.flock(fd, _fcntl.LOCK_UN)
            os.close(fd)
            fd = -1  # Mark as closed so the outer finally doesn't double-close.
            lock_path.unlink(missing_ok=True)
    finally:
        if fd != -1:
            # Closed prematurely (e.g. during acquire failure path).
            os.close(fd)
