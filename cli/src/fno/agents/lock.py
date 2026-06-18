"""fno.agents.lock — per-agent flock context manager.

`hold_agent_lock(name, registry_path, timeout, on_wait)` wraps the
per-agent flock from :mod:`fno.agents.registry` with timeout
semantics and an optional `on_wait` progress callback.

Two-level locking design (US1 architecture):

1. The per-agent flock returned by `_agent_lock_path(name, ...)` serializes
   two `fno agents ask <same-name>` calls end-to-end across subprocess +
   registry write.
2. The registry-wide flock inside `update_registry` serializes the final
   load-modify-write across DIFFERENT agent names.

Lock-release semantics:

- Happy path + most failures: the `finally` branch calls `LOCK_UN` and
  closes the file handle.
- Post-subprocess registry-write failure: the caller invokes
  `handle.detach()` BEFORE re-raising so the `finally` branch skips
  `LOCK_UN`. A stale lock signals "manual cleanup needed" to the next
  caller because a successful `claude --bg` left a supervisor session the
  registry doesn't know about. The file handle is still closed; the lock
  persists only until the process exits or another holder acquires it
  (file-handle close drops POSIX flocks, but the orphan supervisor is the
  real signal — see AC1-FR "registry write fails after subprocess
  success").

For cross-process visibility of `detach()`, callers using subprocess
spawning or PTY supervisors typically hold the lock across the entire
operation in one process, so the file-handle-close-drops-flock semantic
is acceptable. If a future US needs true persistence of the lock past
process exit, switch from `fcntl.flock` to a PID-file or sentinel file.
"""
from __future__ import annotations

import contextlib
import fcntl
import time
from pathlib import Path
from typing import Callable, Iterator, Optional

from fno.agents.registry import _agent_lock_path

# Threshold above which the on_wait callback fires (AC1-UI lock-wait).
_ON_WAIT_THRESHOLD_SECONDS = 1.0

# Poll interval between non-blocking acquire attempts. Small enough to
# honor the 1.0s on_wait threshold within poll granularity, large enough
# to keep CPU usage low for long waits.
_POLL_INTERVAL_SECONDS = 0.05

# Detached file handles are stashed here so POSIX flock stays held until
# process exit. POSIX flocks release when the underlying file descriptor
# closes, so the registry-write-failure path MUST keep its fh alive to
# preserve the "stale lock = manual cleanup needed" signal documented in
# AC1-FR.
#
# CONTRACT: this list is CLI-process-lifetime only. The dispatcher
# raises DispatchAskError(12) immediately after detach(), and the
# typer CLI propagates the exit code to the shell so the process exits
# within milliseconds. A long-lived host (test harness, future daemon)
# that hits multiple detach paths would accumulate fds here unboundedly;
# such a host MUST clear `_detached_handles` between operations or use
# a sentinel-file approach instead. The pytest suite scopes registry
# state to tmp_path via use_tmpdir, so a leaked fd in this list only
# affects the specific lock path that was detached - the next test sees
# a fresh registry directory.
_detached_handles: list[object] = []


class AgentLockTimeout(TimeoutError):
    """Raised when `hold_agent_lock` cannot acquire the flock within timeout."""

    def __init__(self, name: str, timeout: float) -> None:
        super().__init__(f"lock timeout for agent {name!r} after {timeout}s")
        self.name = name
        self.timeout = timeout


class _LockHandle:
    """Yielded inside `hold_agent_lock` — supports `detach()` for the
    registry-write-failure path."""

    __slots__ = ("_detached", "_released")

    def __init__(self) -> None:
        self._detached = False
        self._released = False

    def detach(self) -> None:
        """Suppress the finally branch's `LOCK_UN`.

        Call this BEFORE re-raising when a post-subprocess registry write
        fails, so the stale lock signals "manual cleanup needed" to the
        next caller.
        """
        self._detached = True

    def is_held(self) -> bool:
        """Return True while this process still holds the flock."""
        return not self._released


@contextlib.contextmanager
def hold_agent_lock(
    name: str,
    registry_path: Path,
    *,
    timeout: float = 30.0,
    on_wait: Optional[Callable[[], None]] = None,
) -> Iterator[_LockHandle]:
    """Hold the per-agent flock for the duration of the with-block.

    Args:
        name: Agent name. Rejected (`ValueError`) if it contains path
            separators or `..` — same contract as
            :func:`fno.agents.registry._agent_lock_path`.
        registry_path: Path to the registry file. The flock lives at
            ``<registry-dir>/locks/<name>.lock`` (resolved via
            ``_agent_lock_path``).
        timeout: Acquire timeout in seconds. Default 30. Raises
            :class:`AgentLockTimeout` if not acquired within this window.
        on_wait: Optional zero-arg callback. Called exactly once at or
            after `_ON_WAIT_THRESHOLD_SECONDS` (1.0s) of blocked-acquire
            time. Used by `dispatch_ask` to print
            `Waiting for agent '<name>' lock...` (AC1-UI threshold).

    Yields:
        A :class:`_LockHandle` whose `detach()` method suppresses the
        finally-release. Default behavior releases on exit.
    """
    lock_file = _agent_lock_path(name, registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)

    # Open append-mode so we don't truncate any sentinel a peer process
    # may have written. The flock contract treats the file as a pure
    # sentinel for the OS lock and never reads or writes its contents,
    # so append vs write is purely about avoiding accidental truncation.
    fh = open(lock_file, "a")
    handle = _LockHandle()
    on_wait_fired = False
    start = time.monotonic()
    deadline = start + timeout

    try:
        while True:
            try:
                fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                pass

            now = time.monotonic()
            if now >= deadline:
                raise AgentLockTimeout(name=name, timeout=timeout)

            if (
                not on_wait_fired
                and on_wait is not None
                and (now - start) >= _ON_WAIT_THRESHOLD_SECONDS
            ):
                on_wait()
                on_wait_fired = True

            time.sleep(_POLL_INTERVAL_SECONDS)

        try:
            yield handle
        finally:
            if not handle._detached:
                try:
                    fcntl.flock(fh, fcntl.LOCK_UN)
                except OSError:
                    pass
                handle._released = True
    finally:
        if handle._detached:
            # Stash fh so POSIX flock stays held until process exit.
            _detached_handles.append(fh)
        else:
            fh.close()
