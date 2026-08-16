"""Tests for fno.agents.lock — TDD Red phase for Task 1.1.

Acceptance criteria (US1 task 1.1):

- ``hold_agent_lock(name, registry_path, timeout, on_wait)`` acquires the
  per-agent flock (AC1-HP companion).
- ``on_wait`` callback fires once at or after 1.0s when acquire blocks
  (AC1-UI lock-wait threshold).
- Timeout raises :class:`AgentLockTimeout` (AC1-FR per-agent flock timeout).
- Finally branch releases the lock by default, even on exception.
- ``detach()`` on the yielded handle suppresses release (AC1-FR registry
  write failure — manual cleanup signal).
- Lock path matches ``registry._agent_lock_path(name, registry_path)``.
"""
from __future__ import annotations

import fcntl
import multiprocessing
import time
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Symbol surface
# ---------------------------------------------------------------------------


def test_lock_module_exports() -> None:
    """lock.py exports hold_agent_lock + AgentLockTimeout."""
    from fno.agents import lock as lock_mod

    assert hasattr(lock_mod, "hold_agent_lock")
    assert hasattr(lock_mod, "AgentLockTimeout")


def test_agent_lock_timeout_carries_name_and_timeout() -> None:
    """AgentLockTimeout has .name and .timeout attributes for diagnostics."""
    from fno.agents.lock import AgentLockTimeout

    err = AgentLockTimeout(name="foo", timeout=30)
    assert err.name == "foo"
    assert err.timeout == 30
    assert "foo" in str(err)
    assert "30" in str(err)


@pytest.mark.parametrize("timeout", [float("inf"), float("nan"), -0.1])
def test_hold_agent_lock_rejects_nonterminating_timeout(
    tmp_path: Path, timeout: float
) -> None:
    from fno.agents.lock import hold_agent_lock

    registry_path = tmp_path / "registry.json"
    with pytest.raises(ValueError, match="finite and non-negative"):
        with hold_agent_lock("invalid-budget", registry_path, timeout=timeout):
            pass
    assert not (tmp_path / "locks").exists()


# ---------------------------------------------------------------------------
# Lock path matches registry helper
# ---------------------------------------------------------------------------


def test_hold_agent_lock_uses_registry_lock_path(tmp_path: Path) -> None:
    """The flock file path matches registry._agent_lock_path(name, registry_path)."""
    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    expected = _agent_lock_path("alpha", registry_path)
    assert not expected.exists()

    with hold_agent_lock("alpha", registry_path):
        # File is created by the context manager
        assert expected.exists()


# ---------------------------------------------------------------------------
# Happy path: acquire + release
# ---------------------------------------------------------------------------


def test_hold_agent_lock_acquires_and_releases(tmp_path: Path) -> None:
    """Inside the context, the flock is held; after exit, it can be reacquired."""
    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("beta", registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)

    with hold_agent_lock("beta", registry_path, timeout=5):
        # Try to acquire from a separate file handle non-blocking — must fail.
        with open(lock_file, "w") as fh:
            with pytest.raises(BlockingIOError):
                fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)

    # After context exit the lock must be released so a non-blocking acquire
    # succeeds.
    with open(lock_file, "w") as fh:
        fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(fh, fcntl.LOCK_UN)


def test_hold_agent_lock_releases_on_exception(tmp_path: Path) -> None:
    """Exception inside the with-block does not leak the flock."""
    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("gamma", registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)

    with pytest.raises(RuntimeError, match="boom"):
        with hold_agent_lock("gamma", registry_path, timeout=5):
            raise RuntimeError("boom")

    # Lock released — non-blocking acquire succeeds.
    with open(lock_file, "w") as fh:
        fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(fh, fcntl.LOCK_UN)


# ---------------------------------------------------------------------------
# Detach: post-subprocess registry-write failure path
# ---------------------------------------------------------------------------


def _try_agent_lock(lock_path: str, result_queue) -> None:
    """Child-process probe for whether an existing detached flock is held."""
    with open(lock_path, "a") as fh:
        try:
            fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            result_queue.put("blocked")
            return
        result_queue.put("acquired")
        fcntl.flock(fh, fcntl.LOCK_UN)


def test_hold_agent_lock_detach_suppresses_release(tmp_path: Path) -> None:
    """When the yielded handle.detach() is called, the finally branch does NOT release."""
    import fno.agents.lock as lock_mod
    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("delta", registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)

    with hold_agent_lock("delta", registry_path, timeout=5) as handle:
        handle.detach()

    context = multiprocessing.get_context("spawn")
    blocked_queue = context.Queue()
    blocked_probe = context.Process(
        target=_try_agent_lock,
        args=(str(lock_file), blocked_queue),
    )
    blocked_probe.start()
    assert blocked_queue.get(timeout=5) == "blocked"
    blocked_probe.join(timeout=5)
    assert blocked_probe.exitcode == 0

    retained = lock_mod._detached_handles.pop()
    fcntl.flock(retained, fcntl.LOCK_UN)
    retained.close()

    acquired_queue = context.Queue()
    acquired_probe = context.Process(
        target=_try_agent_lock,
        args=(str(lock_file), acquired_queue),
    )
    acquired_probe.start()
    assert acquired_queue.get(timeout=5) == "acquired"
    acquired_probe.join(timeout=5)
    assert acquired_probe.exitcode == 0


# ---------------------------------------------------------------------------
# on_wait callback
# ---------------------------------------------------------------------------


def _hold_in_child(lock_path: str, hold_seconds: float, ready_path: str) -> None:
    """Helper: acquire the flock and hold it for ``hold_seconds``."""
    import fcntl as _fcntl
    from pathlib import Path as _P

    with open(lock_path, "w") as fh:
        _fcntl.flock(fh, _fcntl.LOCK_EX)
        _P(ready_path).write_text("held")
        time.sleep(hold_seconds)
        _fcntl.flock(fh, _fcntl.LOCK_UN)


def test_on_wait_fires_once_after_1_second(tmp_path: Path) -> None:
    """When acquire blocks for >=1s, on_wait is called exactly once."""
    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("waiter", registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)
    ready = tmp_path / "ready.txt"

    proc = multiprocessing.Process(
        target=_hold_in_child,
        args=(str(lock_file), 1.5, str(ready)),
    )
    proc.start()
    try:
        # Wait for the child to acquire
        deadline = time.monotonic() + 3.0
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert ready.exists(), "child did not acquire lock in time"

        calls: list[float] = []

        def on_wait() -> None:
            calls.append(time.monotonic())

        start = time.monotonic()
        with hold_agent_lock("waiter", registry_path, timeout=10, on_wait=on_wait):
            elapsed = time.monotonic() - start

        # The on_wait callback fired exactly once at or after 1.0s
        assert len(calls) == 1, f"expected 1 on_wait call, got {len(calls)}"
        assert calls[0] - start >= 1.0
        assert calls[0] - start < 1.5  # fired well before the child release at ~1.5s
        # Total acquire took longer than 1s (because child held for 1.5s)
        assert elapsed >= 1.3
    finally:
        proc.join(timeout=5)


def test_on_wait_does_not_fire_when_acquire_is_fast(tmp_path: Path) -> None:
    """When the lock is free, on_wait is NOT called."""
    from fno.agents.lock import hold_agent_lock

    registry_path = tmp_path / "registry.json"
    calls: list[None] = []

    def on_wait() -> None:
        calls.append(None)

    with hold_agent_lock("fast", registry_path, timeout=5, on_wait=on_wait):
        pass

    assert calls == []


# ---------------------------------------------------------------------------
# Timeout
# ---------------------------------------------------------------------------


def test_acquire_timeout_raises_agentlocktimeout(tmp_path: Path) -> None:
    """Acquire that exceeds timeout raises AgentLockTimeout(name, timeout)."""
    from fno.agents.lock import AgentLockTimeout, hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("stuck", registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)
    ready = tmp_path / "ready.txt"

    proc = multiprocessing.Process(
        target=_hold_in_child,
        args=(str(lock_file), 5.0, str(ready)),
    )
    proc.start()
    try:
        deadline = time.monotonic() + 3.0
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert ready.exists()

        start = time.monotonic()
        with pytest.raises(AgentLockTimeout) as exc_info:
            with hold_agent_lock("stuck", registry_path, timeout=1):
                pytest.fail("should not have acquired the lock")
        elapsed = time.monotonic() - start

        assert exc_info.value.name == "stuck"
        assert exc_info.value.timeout == 1
        # Hit the timeout within reasonable bounds
        assert 0.9 <= elapsed <= 2.0
    finally:
        proc.join(timeout=10)


# ---------------------------------------------------------------------------
# Path safety from registry validation
# ---------------------------------------------------------------------------


def test_hold_agent_lock_rejects_path_traversal(tmp_path: Path) -> None:
    """Agent names with path separators or '..' are rejected up-front."""
    from fno.agents.lock import hold_agent_lock

    registry_path = tmp_path / "registry.json"

    for bad in ("../escape", "foo/bar", "..", "a\\b"):
        with pytest.raises(ValueError):
            with hold_agent_lock(bad, registry_path):
                pytest.fail("should not have entered context")


# ---------------------------------------------------------------------------
# Holder stamp: a zero-byte lock is unfalsifiable by inspection
# ---------------------------------------------------------------------------


def _hold_via_api_in_child(
    registry_path: str, name: str, hold_seconds: float, ready_path: str
) -> None:
    """Helper: hold the lock through hold_agent_lock, so the stamp is written."""
    from pathlib import Path as _P

    from fno.agents.lock import hold_agent_lock

    with hold_agent_lock(name, _P(registry_path)):
        _P(ready_path).write_text("held")
        time.sleep(hold_seconds)


def test_acquire_stamps_holder_and_file_does_not_grow(tmp_path: Path) -> None:
    """The lock file carries one JSON line naming pid, name and acquire time."""
    import json
    import os

    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("stamped", registry_path)

    sizes = []
    for _ in range(3):
        with hold_agent_lock("stamped", registry_path):
            raw = lock_file.read_text()
        sizes.append(len(raw))

    assert raw.count("\n") == 1, f"lock file must stay one line: {raw!r}"
    assert len(set(sizes)) == 1, f"lock file grew across acquires: {sizes}"

    holder = json.loads(raw)
    assert holder["pid"] == os.getpid()
    assert holder["name"] == "stamped"
    assert holder["acquired_at"].endswith("+00:00")


def test_release_clears_the_stamp(tmp_path: Path) -> None:
    """A released lock file names nobody.

    The stamp is only true while the flock is held. Leaving it behind restages
    the misreading it exists to end: a released file that still names a pid and
    a time reads as ownership, and the pid is this process, so the liveness
    guard cannot refuse it.
    """
    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("released", registry_path)

    with hold_agent_lock("released", registry_path):
        assert "pid" in lock_file.read_text()

    assert lock_file.read_text() == "", "a free lock must carry no holder"


def test_timeout_names_the_live_holder(tmp_path: Path) -> None:
    """A waiter that gives up reports which pid holds the lock, and since when."""
    from fno.agents.lock import AgentLockTimeout, hold_agent_lock

    registry_path = tmp_path / "registry.json"
    ready = tmp_path / "ready.txt"

    proc = multiprocessing.Process(
        target=_hold_via_api_in_child,
        args=(str(registry_path), "named", 5.0, str(ready)),
    )
    proc.start()
    try:
        deadline = time.monotonic() + 5.0
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert ready.exists(), "child did not acquire lock in time"

        with pytest.raises(AgentLockTimeout) as exc_info:
            with hold_agent_lock("named", registry_path, timeout=1):
                pytest.fail("should not have acquired the lock")

        assert exc_info.value.holder is not None
        assert exc_info.value.holder["pid"] == proc.pid
        assert f"held by pid {proc.pid} since " in str(exc_info.value)
    finally:
        proc.join(timeout=10)


def test_timeout_degrades_when_lock_carries_no_stamp(tmp_path: Path) -> None:
    """An unstamped or corrupt lock file yields the plain message, not a raise."""
    from fno.agents.lock import AgentLockTimeout, hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    registry_path = tmp_path / "registry.json"
    lock_file = _agent_lock_path("bare", registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)
    ready = tmp_path / "ready.txt"

    # _hold_in_child uses a raw flock and writes nothing: the unstamped shape.
    proc = multiprocessing.Process(
        target=_hold_in_child,
        args=(str(lock_file), 5.0, str(ready)),
    )
    proc.start()
    try:
        deadline = time.monotonic() + 5.0
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert ready.exists()

        with pytest.raises(AgentLockTimeout) as exc_info:
            with hold_agent_lock("bare", registry_path, timeout=1):
                pytest.fail("should not have acquired the lock")

        assert exc_info.value.holder is None
        assert str(exc_info.value) == "lock timeout for agent 'bare' after 1s"
    finally:
        proc.join(timeout=10)


def test_holder_note_drops_a_stamp_whose_pid_is_dead() -> None:
    """A stamp can outlive its writer; naming a dead pid is worse than silence.

    `stamp_holder` truncates before it writes, so a write that fails leaves the
    file empty and degrades cleanly. A truncate that fails leaves the PREVIOUS
    holder's line in place, and the waiter then reports a corpse with more
    authority than the bare mtime this stamp replaced.
    """
    import os
    import subprocess
    import sys

    from fno.agents.lock import AgentLockTimeout

    dead = subprocess.Popen([sys.executable, "-c", "pass"])
    dead.wait()

    stale = AgentLockTimeout(
        name="ghost",
        timeout=1.0,
        holder={"pid": dead.pid, "name": "ghost", "acquired_at": "2026-08-15T15:03:11Z"},
    )
    assert stale.holder_note() == ""
    assert str(stale) == "lock timeout for agent 'ghost' after 1.0s"

    live = AgentLockTimeout(
        name="ghost",
        timeout=1.0,
        holder={"pid": os.getpid(), "name": "ghost", "acquired_at": "2026-08-15T15:03:11Z"},
    )
    assert f"held by pid {os.getpid()}" in live.holder_note()


def test_holder_note_past_tense_for_a_caller_that_won_the_lock() -> None:
    """A caller that has since acquired must not name a present owner.

    The grace acquire only succeeds because the holder released, so present
    tense there reports an owner that no longer owns it - and the liveness
    guard cannot refuse that pid, because the process is usually still up.
    """
    import os

    from fno.agents.lock import AgentLockTimeout

    exc = AgentLockTimeout(
        name="red",
        timeout=30.0,
        holder={"pid": os.getpid(), "name": "red", "acquired_at": "2026-08-16T00:00:00Z"},
    )
    assert exc.holder_note().startswith(" (held by pid ")
    assert exc.holder_note(past=True).startswith(" (was held by pid ")
    # An unstamped lock degrades to nothing in either tense.
    bare = AgentLockTimeout(name="red", timeout=30.0, holder=None)
    assert bare.holder_note() == ""
    assert bare.holder_note(past=True) == ""


def test_stamp_timestamp_shape_does_not_vary_with_a_zero_microsecond() -> None:
    """The stamp renders ONE shape, including the once-in-a-million acquire.

    A bare `isoformat()` drops the microseconds field when it is exactly zero,
    writing 25 chars instead of 32. The Rust twin renders one fixed shape and
    its test pins the length, so the drift would show up only on the Python
    side, and only on the rare acquire nobody reproduces.
    """
    import json
    import re
    import tempfile
    from datetime import datetime, timezone

    from fno.agents.lock import hold_agent_lock
    from fno.agents.registry import _agent_lock_path

    with tempfile.TemporaryDirectory() as td:
        reg = Path(td) / "registry.json"
        with hold_agent_lock("shaped", reg):
            stamped = json.loads(_agent_lock_path("shaped", reg).read_text())

    at = stamped["acquired_at"]
    assert re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}\+00:00", at), at
    assert len(at) == 32, at
    # The exact boundary the bare call gets wrong.
    zero_us = datetime(2026, 8, 16, 12, 34, 56, 0, tzinfo=timezone.utc)
    assert len(zero_us.isoformat()) == 25, "the trap this pins is real"
    assert len(zero_us.isoformat(timespec="microseconds")) == 32
