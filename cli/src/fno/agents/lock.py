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
  registry doesn't know about. The process retains the file handle so the
  POSIX flock remains held until that process exits.

This detached lock is a process-lifetime signal, not a durable sentinel:
process exit closes the retained file descriptor and releases the flock.
If a future use needs persistence past process exit, switch from
`fcntl.flock` to a PID-file or sentinel file.
"""
from __future__ import annotations

import contextlib
import fcntl
import json
import os
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterator, Optional

from fno.agents.registry import _agent_lock_path
from fno.time_budget import validate_timeout_budget

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


def _read_holder(lock_file: Path) -> Optional[dict]:
    """Best-effort read of the holder JSON stamped into `lock_file`.

    Returns None on any read or parse failure — an empty, missing, or
    corrupt lock file degrades to the unstamped timeout message rather
    than raising."""
    try:
        with open(lock_file, "r") as fh:
            line = fh.readline()
        if not line.strip():
            return None
        data = json.loads(line)
        return data if isinstance(data, dict) else None
    except (OSError, ValueError):
        return None


class AgentLockTimeout(TimeoutError):
    """Raised when `hold_agent_lock` cannot acquire the flock within timeout."""

    def __init__(
        self, name: str, timeout: float, holder: Optional[dict] = None
    ) -> None:
        self.name = name
        self.timeout = timeout
        self.holder = holder
        super().__init__(
            f"lock timeout for agent {name!r} after {timeout}s{self.holder_note()}"
        )

    def holder_note(self) -> str:
        """`" (held by pid N since T)"`, or `""` when the lock carried no stamp.

        One formatter, because every caller that rebuilds its own message from
        `name` and `timeout` drops the holder otherwise, and the stamp then
        reaches nobody on the path that matters.
        """
        holder = self.holder
        if not holder:
            return ""
        pid = holder.get("pid")
        acquired_at = holder.get("acquired_at")
        if pid is None or not acquired_at:
            return ""
        return f" (held by pid {pid} since {acquired_at})"


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
    validate_timeout_budget(
        timeout,
        label="agent lock",
        poll=_POLL_INTERVAL_SECONDS,
    )
    lock_file = _agent_lock_path(name, registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)

    # Open append-mode so acquiring the flock never truncates a sentinel a
    # peer process may currently hold; the holder JSON is only written
    # (via explicit truncate) once this process actually wins the flock,
    # below.
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
                raise AgentLockTimeout(
                    name=name, timeout=timeout, holder=_read_holder(lock_file)
                )

            if (
                not on_wait_fired
                and on_wait is not None
                and (now - start) >= _ON_WAIT_THRESHOLD_SECONDS
            ):
                on_wait()
                on_wait_fired = True

            time.sleep(_POLL_INTERVAL_SECONDS)

        # Stamp the holder now that the flock is ours. A zero-byte lock is
        # unfalsifiable by inspection, which is how a live 30s wait read to
        # two observers as a 31-hour-old corpse. Truncate first so repeated
        # acquires keep the file at one line instead of growing it.
        try:
            fh.truncate(0)
            fh.write(
                json.dumps(
                    {
                        "pid": os.getpid(),
                        "name": name,
                        "acquired_at": datetime.now(timezone.utc).isoformat(),
                    }
                )
                + "\n"
            )
            fh.flush()
        except OSError:
            pass  # the flock is held either way; the stamp is diagnostic only

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
