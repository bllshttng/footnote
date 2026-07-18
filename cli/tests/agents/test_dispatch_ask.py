"""Tests for fno.agents.dispatch.dispatch_ask — Task 1.3.

Covers AC1-HP / AC1-ERR / AC1-UI / AC1-EDGE / AC1-FR from the design
doc. Provider invocation is monkeypatched via the fake-claude script
installed in test-isolated bin dirs OR via direct ``_subprocess_run``
patching on ``providers.claude``.
"""
from __future__ import annotations

import re
import time
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from fno.paths_testing import use_tmpdir
from tests.agents._fake_claude import configure_fake, install_fake_claude


def _install_fake(tmp_path: Path, monkeypatch) -> Path:
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)
    return bin_dir


def _parallel_dispatch_worker(
    home: str, name: str, message: str, ready_path: str, gate_path: str
) -> None:
    """Module-level multiprocessing target — runs `dispatch_ask` once.

    Used by the parallel-same-name race test. Reports readiness via
    `ready_path` (touched on entry) then blocks until `gate_path` exists
    so the test driver can land two workers within the same lock window.
    Calls ``sys.exit`` with 0 on success or DispatchAskError.exit_code
    on failure — Process.exitcode reads the OS exit code, not the
    target function's return value.
    """
    import os as _os
    import sys as _sys
    import time as _time
    from pathlib import Path as _P

    _os.environ["HOME"] = home
    _os.environ["FNO_CONFIG"] = str(_P(home) / ".fno" / "settings.yaml")

    # Reset the paths module's @cache so the test-isolated settings apply
    # in the child process (fork-spawned children inherit the parent's
    # cache state, which points at the parent's PYTHONPATH config).
    from fno import paths as _paths

    if hasattr(_paths._settings, "cache_clear"):
        _paths._settings.cache_clear()
    if hasattr(_paths.resolve_repo_root, "cache_clear"):
        _paths.resolve_repo_root.cache_clear()

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    _P(ready_path).write_text("ready")
    while not _P(gate_path).exists():
        _time.sleep(0.02)

    try:
        dispatch_ask(
            name=name,
            message=message,
            provider="claude",
            cwd=_P(home),
            timeout=10,
        )
        _sys.exit(0)
    except DispatchAskError as exc:
        _sys.exit(exc.exit_code)


def _hold_lock_until_release(lock_path: str, ready: str, release: str) -> None:
    """Module-level multiprocessing target — must be importable for `spawn` start method.

    Acquires the per-agent flock at `lock_path`, signals readiness, then
    holds until the `release` sentinel file appears.
    """
    import fcntl as _fcntl
    import time as _time
    from pathlib import Path as _P

    with open(lock_path, "w") as fh:
        _fcntl.flock(fh, _fcntl.LOCK_EX)
        _P(ready).write_text("held")
        while not _P(release).exists():
            _time.sleep(0.05)
        _fcntl.flock(fh, _fcntl.LOCK_UN)


# ---------------------------------------------------------------------------
# Symbol surface
# ---------------------------------------------------------------------------


def test_dispatch_module_exports_dispatch_ask() -> None:
    from fno.agents import dispatch

    assert hasattr(dispatch, "dispatch_ask")


# ---------------------------------------------------------------------------
# AC1-HP — happy path
# ---------------------------------------------------------------------------


def test_dispatch_ask_happy_path(tmp_path: Path, monkeypatch) -> None:
    """AC1-HP (post-bus-group1): ask on an unknown name now returns unknown-agent
    error (exit 16). The create contract moved to the spawn verb (Task 1.2).
    This test verifies ask rejects the unknown name cleanly."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask, UNKNOWN_AGENT_EXIT_CODE

    cwd = tmp_path / "workdir"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="frontend-worker",
            message="implement Login.tsx",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )
    assert exc_info.value.exit_code == UNKNOWN_AGENT_EXIT_CODE
    assert "unknown agent" in str(exc_info.value)
    assert "spawn it first" in str(exc_info.value)


def test_claude_create_path_happy_path(tmp_path: Path, monkeypatch) -> None:
    """AC1-HP machinery: _claude_create_path creates registry entry, emits events,
    returns kind='create'. Repoints the old dispatch_ask create test at the helper."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno import paths
    from fno.agents.dispatch import _claude_create_path
    from fno.agents.registry import load_registry, _agent_lock_path
    import fcntl as _fcntl

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    # _claude_create_path runs INSIDE the per-agent flock. Acquire a real flock
    # handle and pass it in so the helper's lock_handle.detach() path is reachable.
    registry_path = paths.agents_registry_path()
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = _agent_lock_path("frontend-worker", registry_path)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    class _FakeLockHandle:
        def detach(self) -> None:
            pass

    result = _claude_create_path(
        name="frontend-worker",
        message="implement Login.tsx",
        cwd=cwd,
        chosen="claude",
        timeout=10,
        yolo=False,
        lock_handle=_FakeLockHandle(),
    )

    assert result.kind == "create"
    assert result.short_id == "7c5dcf5d"

    entries = load_registry()
    assert len(entries) == 1
    entry = entries[0]
    assert entry.name == "frontend-worker"
    assert entry.harness == "claude"
    assert entry.short_id == "7c5dcf5d"
    assert entry.cwd == str(cwd)

    events_log = paths.state_dir() / "events.jsonl"
    body = events_log.read_text(encoding="utf-8")
    assert "agent_ask_done" in body
    assert "7c5dcf5d" in body


# ---------------------------------------------------------------------------
# AC1-ERR — input validation
# ---------------------------------------------------------------------------


def test_dispatch_ask_rejects_empty_message(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR empty message: exit 2 with `message must be non-empty`."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "w"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(name="z", message="", provider="claude", cwd=cwd, timeout=10)
    assert exc_info.value.exit_code == 2
    assert "message must be non-empty" in str(exc_info.value)

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(name="z", message="   ", provider="claude", cwd=cwd, timeout=10)
    assert exc_info.value.exit_code == 2


def test_dispatch_ask_rejects_name_too_long(tmp_path: Path, monkeypatch) -> None:
    """AC1-EDGE: 129+ chars rejected with exit 2 and `name must be <=128 chars`."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "w"
    cwd.mkdir()
    long_name = "a" * 129
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name=long_name,
            message="hi",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )
    assert exc_info.value.exit_code == 2
    assert "name must be <=128 chars" in str(exc_info.value)


def test_dispatch_ask_accepts_name_at_128_chars(tmp_path: Path, monkeypatch) -> None:
    """AC1-EDGE: exactly 128 chars passes input validation (then exits 16 for unknown name)."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask, UNKNOWN_AGENT_EXIT_CODE

    cwd = tmp_path / "w"
    cwd.mkdir()
    name = "a" * 128
    # 128-char name is valid input; unknown-agent error fires (not input-validation error)
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(name=name, message="hi", provider="claude", cwd=cwd, timeout=10)
    # Exit 16 (not 2) proves the name passed input validation and was rejected as unknown
    assert exc_info.value.exit_code == UNKNOWN_AGENT_EXIT_CODE


def test_dispatch_ask_rejects_name_matching_short_id_shape(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-EDGE: agent names matching ^[0-9a-f]{8}$ are rejected for collision safety."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "w"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="7c5dcf5d", message="hi", provider="claude", cwd=cwd, timeout=10
        )
    assert exc_info.value.exit_code == 2
    assert "short-id shape" in str(exc_info.value)


def test_dispatch_ask_rejects_unknown_provider(tmp_path: Path, monkeypatch) -> None:
    """Unknown provider name: for an unknown agent, exit 16 fires before provider validation.
    For an existing agent with wrong provider, exit 2 (ProviderMismatchError) fires.
    This test pins the unknown-agent-first ordering."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask, UNKNOWN_AGENT_EXIT_CODE

    cwd = tmp_path / "w"
    cwd.mkdir()
    # Unknown name + invalid provider -> exit 16 (unknown-agent check precedes provider check)
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="x",
            message="hi",
            provider="not-real",
            cwd=cwd,
            timeout=10,
        )
    assert exc_info.value.exit_code == UNKNOWN_AGENT_EXIT_CODE


def test_dispatch_ask_claude_not_on_path(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR: unknown agent -> exit 16 regardless of PATH (unknown-agent check is first).
    The PATH check (exit 14) is now in _claude_create_path / spawn path, not dispatch_ask."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("PATH", "/nonexistent-bin-dir-for-test")

    from fno.agents.dispatch import DispatchAskError, dispatch_ask, UNKNOWN_AGENT_EXIT_CODE

    cwd = tmp_path / "w"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="y", message="hi", provider="claude", cwd=cwd, timeout=10
        )
    # Exit 16 (unknown-agent) precedes PATH check now; PATH=14 belongs to spawn
    assert exc_info.value.exit_code == UNKNOWN_AGENT_EXIT_CODE


def test_dispatch_ask_existing_name_routes_to_followup(tmp_path: Path, monkeypatch) -> None:
    """US2 supersedes US1 LD #2: existing name routes to follow-up path.

    With no real claude session present, follow-up fails fast at
    locate_session → ProviderOrphanError(reason="not-found") → exit 13.
    The point of this test is to prove the routing happens (not exit 2).
    """
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    # Point HOME at a directory with no ~/.claude/sessions so locate_session
    # returns None → orphan.
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    (tmp_path / "home").mkdir()

    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [
            AgentEntry(
                name="already-there",
                harness="claude",
                cwd="/tmp",
                log_path="/tmp/a.log",
                short_id="abc12345",
            )
        ]
    )

    cwd = tmp_path / "w"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="already-there",
            message="new msg",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )
    # Exit 13 = orphan; routing reached ask_followup, NOT the US1 reject path.
    assert exc_info.value.exit_code == 13
    msg = str(exc_info.value)
    assert "already-there" in msg
    assert "not running" in msg


def test_dispatch_ask_new_agent_requires_provider(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-ERR: unknown agent with no provider -> exit 16 (unknown-agent precedes
    provider-required check). Provider is only required for spawn/host."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask, UNKNOWN_AGENT_EXIT_CODE

    cwd = tmp_path / "w"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(name="x", message="hi", provider=None, cwd=cwd, timeout=10)
    # Unknown-agent guard fires before select_provider's "provider is required" check
    assert exc_info.value.exit_code == UNKNOWN_AGENT_EXIT_CODE
    assert "unknown agent" in str(exc_info.value)


# ---------------------------------------------------------------------------
# AC1-FR — registry-write failure preserves lock + emits orphan event
# ---------------------------------------------------------------------------


def test_dispatch_ask_preserves_lock_on_registry_failure(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-FR: _claude_create_path: post-subprocess registry write OSError → exit 12,
    lock detached, orphan event + stderr surface the short_id.
    Repointed at the extracted helper (create contract moved to spawn verb)."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno import paths
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskError, _claude_create_path

    # Force update_registry to raise OSError after the subprocess succeeds
    def boom(updater, path=None):  # type: ignore[no-untyped-def]
        raise OSError("No space left on device")

    monkeypatch.setattr(dispatch_mod, "update_registry", boom)

    cwd = tmp_path / "w"
    cwd.mkdir()

    detach_called: list[bool] = []

    class _TrackedLockHandle:
        def detach(self) -> None:
            detach_called.append(True)

    with pytest.raises(DispatchAskError) as exc_info:
        _claude_create_path(
            name="doomed",
            message="hi",
            cwd=cwd,
            chosen="claude",
            timeout=10,
            yolo=False,
            lock_handle=_TrackedLockHandle(),
        )

    err = exc_info.value
    assert err.exit_code == 12
    msg = str(err)
    assert "registry write failed" in msg
    assert "No space left on device" in msg
    assert "7c5dcf5d" in msg  # orphan short-id surfaced
    assert "claude rm 7c5dcf5d" in msg

    # lock_handle.detach() must have been called (AC1-FR lock semantics)
    assert detach_called, "lock_handle.detach() must be called on registry failure"

    # And the events log records the registry-write failure with short_id
    events_log = paths.state_dir() / "events.jsonl"
    body = events_log.read_text(encoding="utf-8")
    assert "agent_ask_failed" in body
    assert "registry-write" in body
    assert "7c5dcf5d" in body


# ---------------------------------------------------------------------------
# AC1-FR — subprocess non-zero + parse failure
# ---------------------------------------------------------------------------


def test_dispatch_ask_subprocess_nonzero(tmp_path: Path, monkeypatch) -> None:
    """AC1-FR: _claude_create_path: claude --bg exits 1 → exit 1, verbatim stderr, no registry write.
    Repointed at the extracted helper (create contract moved to spawn verb)."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(
        monkeypatch,
        exit_code=1,
        stderr="Error: not authenticated. Run claude /login\n",
    )

    from fno import paths
    from fno.agents.dispatch import DispatchAskError, _claude_create_path
    from fno.agents.registry import load_registry

    cwd = tmp_path / "w"
    cwd.mkdir()

    class _FakeLockHandle:
        def detach(self) -> None:
            pass

    with pytest.raises(DispatchAskError) as exc_info:
        _claude_create_path(
            name="x",
            message="hi",
            cwd=cwd,
            chosen="claude",
            timeout=10,
            yolo=False,
            lock_handle=_FakeLockHandle(),
        )

    assert exc_info.value.exit_code == 1
    assert "not authenticated" in str(exc_info.value)

    assert load_registry() == []
    events_log = paths.state_dir() / "events.jsonl"
    body = events_log.read_text(encoding="utf-8")
    assert "agent_ask_failed" in body
    assert "subprocess" in body


def test_dispatch_ask_parse_failure(tmp_path: Path, monkeypatch) -> None:
    """AC1-FR: _claude_create_path: garbage stdout → exit 1, first 200 chars surfaced, no registry write.
    Repointed at the extracted helper (create contract moved to spawn verb)."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch, stdout="Session created: foo-bar\n")

    from fno import paths
    from fno.agents.dispatch import DispatchAskError, _claude_create_path
    from fno.agents.registry import load_registry

    cwd = tmp_path / "w"
    cwd.mkdir()

    class _FakeLockHandle:
        def detach(self) -> None:
            pass

    with pytest.raises(DispatchAskError) as exc_info:
        _claude_create_path(
            name="x",
            message="hi",
            cwd=cwd,
            chosen="claude",
            timeout=10,
            yolo=False,
            lock_handle=_FakeLockHandle(),
        )

    assert exc_info.value.exit_code == 1
    assert "unable to parse short-id" in str(exc_info.value)
    assert "Session created" in str(exc_info.value)

    assert load_registry() == []
    events_log = paths.state_dir() / "events.jsonl"
    body = events_log.read_text(encoding="utf-8")
    assert "agent_ask_failed" in body
    assert "parse" in body


# ---------------------------------------------------------------------------
# AC1-FR per-agent flock timeout
# ---------------------------------------------------------------------------


def test_dispatch_ask_lock_timeout(tmp_path: Path, monkeypatch) -> None:
    """AC1-FR: per-agent flock timeout → exit 11, no subprocess invoked."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno import paths
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.registry import _agent_lock_path

    # Externally hold the lock for the agent name "stuck" via a separate
    # process to force a timeout for dispatch_ask.
    import multiprocessing

    registry_path = paths.agents_registry_path()
    # Ensure registry dir exists (the locks/ subdir is created on demand)
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = _agent_lock_path("stuck", registry_path)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    ready = tmp_path / "ready"
    release = tmp_path / "release"
    proc = multiprocessing.Process(
        target=_hold_lock_until_release,
        args=(str(lock_path), str(ready), str(release)),
    )
    proc.start()
    try:
        deadline = time.monotonic() + 5.0
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert ready.exists()

        cwd = tmp_path / "w"
        cwd.mkdir()
        with pytest.raises(DispatchAskError) as exc_info:
            dispatch_ask(
                name="stuck",
                message="hi",
                provider="claude",
                cwd=cwd,
                timeout=10,
                lock_timeout=1,
            )

        assert exc_info.value.exit_code == 11
        assert "lock timeout" in str(exc_info.value)
        assert "stuck" in str(exc_info.value)

        events_log = paths.state_dir() / "events.jsonl"
        body = events_log.read_text(encoding="utf-8")
        assert "lock-timeout" in body
    finally:
        release.write_text("go")
        proc.join(timeout=10)


# ---------------------------------------------------------------------------
# AC1-UI — wait message on slow lock acquire
# ---------------------------------------------------------------------------


def test_dispatch_ask_prints_wait_message(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC1-UI: stderr prints `Waiting for agent '<name>' lock...` once when
    lock acquire takes >=1s."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno import paths
    from fno.agents.dispatch import dispatch_ask
    from fno.agents.registry import _agent_lock_path

    import multiprocessing

    registry_path = paths.agents_registry_path()
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = _agent_lock_path("slow", registry_path)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    ready = tmp_path / "ready"
    release = tmp_path / "release"
    proc = multiprocessing.Process(
        target=_hold_lock_until_release,
        args=(str(lock_path), str(ready), str(release)),
    )
    proc.start()
    try:
        deadline = time.monotonic() + 5.0
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert ready.exists()

        # Schedule release after 1.5s so dispatch_ask hits the on_wait
        # threshold then acquires.
        import threading

        def release_later() -> None:
            time.sleep(1.5)
            release.write_text("go")

        threading.Thread(target=release_later, daemon=True).start()

        cwd = tmp_path / "w"
        cwd.mkdir()
        # After de-overloading ask: unknown agent "slow" -> exit 16.
        # The wait message test still validates the lock-wait behavior:
        # dispatch_ask acquires the lock, THEN hits the unknown-agent guard.
        # The wait message fires BEFORE the unknown-agent check, so it is
        # still observable even though the result is an error.
        from fno.agents.dispatch import DispatchAskError, UNKNOWN_AGENT_EXIT_CODE

        with pytest.raises(DispatchAskError) as exc_info:
            dispatch_ask(
                name="slow",
                message="hi",
                provider="claude",
                cwd=cwd,
                timeout=10,
                lock_timeout=10,
            )
        assert exc_info.value.exit_code == UNKNOWN_AGENT_EXIT_CODE

        captured = capsys.readouterr()
        # The wait message must fire exactly once on stderr
        wait_lines = [
            line for line in captured.err.splitlines() if "Waiting for agent" in line
        ]
        assert len(wait_lines) == 1
        assert "'slow'" in wait_lines[0]
    finally:
        release.write_text("go")
        proc.join(timeout=10)


# ---------------------------------------------------------------------------
# Architecture step 3: select_provider runs INSIDE the per-agent flock
# ---------------------------------------------------------------------------


def test_dispatch_ask_select_provider_inside_flock(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-EDGE concurrent same-name: select_provider must run INSIDE the
    per-agent flock so the second call sees the first's write.
    Pre-registers the agent so dispatch_ask reaches select_provider (not unknown-agent guard)."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import dispatch_ask, DispatchAskError
    from fno.agents.registry import AgentEntry, write_registry

    # Pre-register the agent so dispatch_ask routes to follow-up (not unknown-agent)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    (tmp_path / "home").mkdir()
    write_registry([
        AgentEntry(
            name="ordered",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/a.log",
            short_id="abc12345",
        )
    ])

    # Order log: hold_agent_lock acquire → select_provider invocation
    order: list[str] = []

    original_select = dispatch_mod.select_provider
    original_hold = dispatch_mod.hold_agent_lock

    def traced_select(name, requested_provider):  # type: ignore[no-untyped-def]
        order.append("select_provider")
        return original_select(name=name, requested_provider=requested_provider)

    import contextlib as _ctx

    @_ctx.contextmanager
    def traced_hold(*args, **kwargs):  # type: ignore[no-untyped-def]
        order.append("hold_lock_enter")
        with original_hold(*args, **kwargs) as handle:
            order.append("hold_lock_acquired")
            yield handle
        order.append("hold_lock_exit")

    monkeypatch.setattr(dispatch_mod, "select_provider", traced_select)
    monkeypatch.setattr(dispatch_mod, "hold_agent_lock", traced_hold)

    cwd = tmp_path / "w"
    cwd.mkdir()
    # Follow-up will fail at orphan stage (no real claude session) - that's fine.
    # The assertion is on the ORDER of lock+select_provider, not on success.
    with pytest.raises(DispatchAskError):
        dispatch_ask(
            name="ordered", message="hi", provider="claude", cwd=cwd, timeout=10
        )

    # The trace MUST show the lock acquired before select_provider runs.
    assert "hold_lock_acquired" in order, f"lock was never acquired: {order}"
    assert "select_provider" in order, f"select_provider was never called: {order}"
    acquired_idx = order.index("hold_lock_acquired")
    select_idx = order.index("select_provider")
    assert acquired_idx < select_idx, f"order was {order}"


# ---------------------------------------------------------------------------
# AC1-FR Ctrl-C during subprocess
# ---------------------------------------------------------------------------


def test_dispatch_ask_registry_version_error_surfaces_as_exit_12(
    tmp_path: Path, monkeypatch
) -> None:
    """A RegistryVersionError from the in-lock load_registry MUST surface
    as DispatchAskError(12), not a raw RuntimeError. Catches the
    `except (OSError, ValueError)` gap Gemini flagged on PR review."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno import paths
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.registry import RegistryVersionError

    def boom():  # type: ignore[no-untyped-def]
        raise RegistryVersionError("schema_version mismatch")

    monkeypatch.setattr(dispatch_mod, "load_registry", boom)

    cwd = tmp_path / "w"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="rve",
            message="hi",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )

    assert exc_info.value.exit_code == 12
    assert "registry read failed" in str(exc_info.value)
    assert "schema_version" in str(exc_info.value)

    events_log = paths.state_dir() / "events.jsonl"
    body = events_log.read_text(encoding="utf-8")
    assert "registry-read" in body


def test_dispatch_ask_handles_ctrl_c(tmp_path: Path, monkeypatch) -> None:
    """AC1-FR: KeyboardInterrupt from _claude_create_path propagates through
    the flock context, lock is released, registry unchanged.
    Repointed at the extracted helper (create contract moved to spawn verb)."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)

    from fno import paths
    from fno.agents.dispatch import _claude_create_path
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import _agent_lock_path, load_registry

    def boom(*args, **kwargs):  # type: ignore[no-untyped-def]
        raise KeyboardInterrupt("user pressed ctrl-c")

    monkeypatch.setattr(claude_mod, "_subprocess_run", boom)

    cwd = tmp_path / "w"
    cwd.mkdir()

    class _FakeLockHandle:
        def detach(self) -> None:
            pass

    with pytest.raises(KeyboardInterrupt):
        _claude_create_path(
            name="ctrlc",
            message="hi",
            cwd=cwd,
            chosen="claude",
            timeout=10,
            yolo=False,
            lock_handle=_FakeLockHandle(),
        )

    # Registry unchanged
    assert load_registry() == []


# ---------------------------------------------------------------------------
# AC1-EDGE — real 2-process same-name race
# ---------------------------------------------------------------------------


def test_dispatch_ask_two_processes_same_name(tmp_path: Path, monkeypatch) -> None:
    """AC1-EDGE (post-bus-group1): two parallel `dispatch_ask` calls with the SAME
    unknown name both exit 16 (unknown-agent). Neither creates a registry entry.

    The test exercises the flock serialization path: both workers race for the
    per-agent lock, both acquire it in sequence, and both see no existing entry
    so both get UNKNOWN_AGENT_EXIT_CODE (16). Registry stays empty.
    """
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = _install_fake(tmp_path, monkeypatch)

    import multiprocessing

    home = str(tmp_path)
    ready_a = tmp_path / "ready_a"
    ready_b = tmp_path / "ready_b"
    gate = tmp_path / "gate"

    monkeypatch.setenv("PATH", str(bin_dir))

    ctx = multiprocessing.get_context("fork")
    p_a = ctx.Process(
        target=_parallel_dispatch_worker,
        args=(home, "shared", "msg-a", str(ready_a), str(gate)),
    )
    p_b = ctx.Process(
        target=_parallel_dispatch_worker,
        args=(home, "shared", "msg-b", str(ready_b), str(gate)),
    )

    try:
        p_a.start()
        p_b.start()

        deadline = time.monotonic() + 10
        while (not ready_a.exists() or not ready_b.exists()) and time.monotonic() < deadline:
            time.sleep(0.05)
        assert ready_a.exists() and ready_b.exists(), "workers did not signal ready"

        gate.write_text("go")

        p_a.join(timeout=15)
        p_b.join(timeout=15)
        assert p_a.exitcode is not None
        assert p_b.exitcode is not None

        # Both workers see unknown-agent (exit 16) since neither pre-created the agent
        from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

        assert p_a.exitcode == UNKNOWN_AGENT_EXIT_CODE, f"a={p_a.exitcode}"
        assert p_b.exitcode == UNKNOWN_AGENT_EXIT_CODE, f"b={p_b.exitcode}"

        # Registry must be empty (no creation happened)
        from fno.agents.registry import load_registry

        entries = [e for e in load_registry() if e.name == "shared"]
        assert entries == [], f"Registry must be empty, got {entries}"
    finally:
        for p in (p_a, p_b):
            if p.is_alive():
                p.terminate()
                p.join(timeout=5)


# ---------------------------------------------------------------------------
# US2: follow-up integration tests
# ---------------------------------------------------------------------------


def _seed_followup_target(tmp_path: Path, name: str = "frontend-worker") -> None:
    """Seed the registry with one Claude agent ready for follow-up routing."""
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            harness="claude",
            cwd="/tmp",
            log_path=f"/tmp/{name}.log",
            short_id="abc12345",
            status="live",
        )
    ])


def test_dispatch_ask_followup_happy_path(tmp_path: Path, monkeypatch) -> None:
    """AC2-HP: existing-name routes to follow-up; reply returned, registry bumped,
    events emitted."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    from fno import paths
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import dispatch_ask
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    captured: dict = {}

    def fake_ask_followup(*, claude_short_id, message, cwd, from_name,
                          timeout, poll_interval=0.5, jobs_dir=None):
        captured["short_id"] = claude_short_id
        captured["message"] = message
        captured["from_name"] = from_name
        return "validation added"

    monkeypatch.setattr(claude_mod, "ask_followup", fake_ask_followup)

    result = dispatch_ask(
        name="frontend-worker",
        message="add validation",
        provider="claude",
        cwd=tmp_path,
        timeout=30,
    )

    assert result.kind == "followup"
    assert result.short_id == "abc12345"
    assert result.reply == "validation added"
    assert captured["short_id"] == "abc12345"
    assert captured["message"] == "add validation"
    assert captured["from_name"] == "fno"

    entries = load_registry()
    target = next(e for e in entries if e.name == "frontend-worker")
    assert target.status == "live"
    assert target.last_message_at is not None

    events_body = (paths.state_dir() / "events.jsonl").read_text(encoding="utf-8")
    assert "agent_followup_started" in events_body
    assert "agent_followup_done" in events_body


def test_dispatch_ask_followup_orphan_marks_status(tmp_path: Path, monkeypatch) -> None:
    """AC2-ERR: orphan on follow-up → exit 13, registry status="orphaned"."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    def fake_ask_followup(**kwargs):  # type: ignore[no-untyped-def]
        raise claude_mod.ProviderOrphanError(
            reason="not-found", short_id="abc12345"
        )

    monkeypatch.setattr(claude_mod, "ask_followup", fake_ask_followup)

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="frontend-worker",
            message="msg",
            provider="claude",
            cwd=tmp_path,
            timeout=10,
        )

    assert exc_info.value.exit_code == 13
    assert "not running" in str(exc_info.value)
    assert "not-found" in str(exc_info.value)

    entries = load_registry()
    target = next(e for e in entries if e.name == "frontend-worker")
    assert target.status == "orphaned"


def test_dispatch_ask_followup_provider_mismatch(tmp_path: Path, monkeypatch) -> None:
    """AC2-ERR: --provider gemini on a claude agent → exit 2 via select_provider."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="frontend-worker",
            message="msg",
            provider="gemini",
            cwd=tmp_path,
            timeout=10,
        )
    assert exc_info.value.exit_code == 2
    assert "refusing to follow-up as provider=gemini" in str(exc_info.value)


def test_dispatch_ask_followup_poll_timeout(tmp_path: Path, monkeypatch) -> None:
    """AC2-FR: ProviderTimeoutError → exit 15; last_message_at unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    def fake_ask_followup(**kwargs):  # type: ignore[no-untyped-def]
        raise claude_mod.ProviderTimeoutError(elapsed_sec=600.0,
                                              short_id="abc12345")

    monkeypatch.setattr(claude_mod, "ask_followup", fake_ask_followup)

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="frontend-worker",
            message="msg",
            provider="claude",
            cwd=tmp_path,
            timeout=10,
        )

    assert exc_info.value.exit_code == 15
    assert "no reply within" in str(exc_info.value)
    assert "fno agents logs" in str(exc_info.value)

    # last_message_at NOT updated (we do not know if reply landed)
    entries = load_registry()
    target = next(e for e in entries if e.name == "frontend-worker")
    assert target.last_message_at is None


def test_dispatch_ask_followup_socket_error(tmp_path: Path, monkeypatch) -> None:
    """AC2-FR: ProviderSocketError → exit 1; registry unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    def fake_ask_followup(**kwargs):  # type: ignore[no-untyped-def]
        raise claude_mod.ProviderSocketError("Broken pipe")

    monkeypatch.setattr(claude_mod, "ask_followup", fake_ask_followup)

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="frontend-worker",
            message="msg",
            provider="claude",
            cwd=tmp_path,
            timeout=10,
        )
    assert exc_info.value.exit_code == 1
    assert "Broken pipe" in str(exc_info.value)

    entries = load_registry()
    target = next(e for e in entries if e.name == "frontend-worker")
    assert target.last_message_at is None
    assert target.status == "live"  # not touched


def test_dispatch_ask_followup_preserves_lock_on_registry_write_failure(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-FR: update_registry OSError after successful send → exit 12, lock held,
    reply NOT printed (no stdout leak — dispatch_ask raises before return)."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    import fcntl

    from fno import paths
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import _agent_lock_path

    def fake_ask_followup(**kwargs):  # type: ignore[no-untyped-def]
        return "got it, working on it"

    # First update_registry call is the post-send status bump. Make it raise.
    monkeypatch.setattr(claude_mod, "ask_followup", fake_ask_followup)

    real_update = dispatch_mod.update_registry
    raise_count = {"n": 0}

    def boom(updater, path=None):  # type: ignore[no-untyped-def]
        raise_count["n"] += 1
        raise OSError("No space left on device")

    monkeypatch.setattr(dispatch_mod, "update_registry", boom)

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="frontend-worker",
            message="msg",
            provider="claude",
            cwd=tmp_path,
            timeout=10,
        )

    assert exc_info.value.exit_code == 12
    msg = str(exc_info.value)
    assert "registry write failed" in msg
    assert "do not retry" in msg

    # Lock MUST still be held — AC2-FR manual-cleanup signal.
    # No `if exists` guard: hold_agent_lock creates the file before
    # acquiring, so absence is a regression we want to fail loudly on
    # (matching the US1 equivalent at test_dispatch_ask_preserves_lock_on_registry_failure).
    registry_path = paths.agents_registry_path()
    lock_file = _agent_lock_path("frontend-worker", registry_path)
    with open(lock_file, "w") as fh:
        with pytest.raises(BlockingIOError):
            fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)


def test_dispatch_ask_followup_rejects_xml_unsafe_from_name(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-ERR: --from-name with XML-unsafe chars → exit 2; no provider call."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.providers import claude as claude_mod

    called = {"n": 0}

    def trap_ask_followup(**kwargs):  # type: ignore[no-untyped-def]
        called["n"] += 1
        return "should not get here"

    monkeypatch.setattr(claude_mod, "ask_followup", trap_ask_followup)

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="frontend-worker",
            message="msg",
            provider="claude",
            cwd=tmp_path,
            timeout=10,
            from_name='bad"name',
        )
    assert exc_info.value.exit_code == 2
    assert "XML-unsafe" in str(exc_info.value)
    assert called["n"] == 0


def test_dispatch_ask_followup_ctrl_c_during_poll_releases_lock(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-FR Ctrl-C during poll loop: KeyboardInterrupt propagates,
    flock is released by the finally branch, registry's last_message_at
    is NOT updated (we don't know if the reply landed), reply is NOT
    printed to stdout."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    import fcntl

    from fno import paths
    from fno.agents.dispatch import dispatch_ask
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import _agent_lock_path, load_registry

    def boom(**kwargs):  # type: ignore[no-untyped-def]
        # Simulate the user pressing Ctrl-C mid-poll: ask_followup is
        # synchronous from the caller's view, so a SIGINT during its
        # sleep() loop raises KeyboardInterrupt at this layer.
        raise KeyboardInterrupt("user pressed ctrl-c")

    monkeypatch.setattr(claude_mod, "ask_followup", boom)

    with pytest.raises(KeyboardInterrupt):
        dispatch_ask(
            name="frontend-worker",
            message="msg",
            provider="claude",
            cwd=tmp_path,
            timeout=10,
        )

    # Registry's last_message_at unchanged — at-least-once semantics
    # from the orchestrator's view (we cannot confirm reply landed).
    entries = load_registry()
    target = next(e for e in entries if e.name == "frontend-worker")
    assert target.last_message_at is None
    assert target.status == "live"  # untouched

    # Per-agent flock released (the finally branch in hold_agent_lock
    # ran LOCK_UN + close): a non-blocking acquire from a fresh handle
    # in this same process succeeds.
    registry_path = paths.agents_registry_path()
    lock_file = _agent_lock_path("frontend-worker", registry_path)
    if lock_file.exists():
        with open(lock_file, "w") as fh:
            fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(fh, fcntl.LOCK_UN)


def test_dispatch_ask_followup_default_from_name_is_abilities(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-EDGE: default from_name='fno' reaches the provider call."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake(tmp_path, monkeypatch)
    _seed_followup_target(tmp_path)

    from fno.agents.dispatch import dispatch_ask
    from fno.agents.providers import claude as claude_mod

    captured: dict = {}

    def fake_ask_followup(**kwargs):  # type: ignore[no-untyped-def]
        captured.update(kwargs)
        return "ok"

    monkeypatch.setattr(claude_mod, "ask_followup", fake_ask_followup)

    dispatch_ask(
        name="frontend-worker",
        message="msg",
        provider="claude",
        cwd=tmp_path,
        timeout=10,
    )
    assert captured["from_name"] == "fno"


# --- _inside_leg_is_recent (x-c393): provably-live guard signal ----------


def test_inside_leg_is_recent_true_for_fresh_report():
    """A report stamped near `now` counts as provably-live (AC2-HP)."""
    from fno.agents.dispatch import _inside_leg_is_recent

    now = 1_000_000.0
    stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 30))
    assert _inside_leg_is_recent({"received_at": stamp}, now) is True


def test_inside_leg_is_recent_false_when_absent():
    """No inside_leg report -> not provably live (a routing miss orphans)."""
    from fno.agents.dispatch import _inside_leg_is_recent

    assert _inside_leg_is_recent(None, 1_000_000.0) is False


def test_inside_leg_is_recent_false_when_stale():
    """A report older than the window is not a liveness signal (AC2-ERR side)."""
    from fno.agents.dispatch import _inside_leg_is_recent, _PROVABLY_LIVE_WINDOW_SEC

    now = 1_000_000.0
    stamp = time.strftime(
        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - _PROVABLY_LIVE_WINDOW_SEC - 60)
    )
    assert _inside_leg_is_recent({"received_at": stamp}, now) is False


def test_inside_leg_is_recent_false_on_unparseable_stamp():
    """A corrupt stamp fails closed -> not recent (never shields a dead row)."""
    from fno.agents.dispatch import _inside_leg_is_recent

    assert _inside_leg_is_recent({"received_at": "not-a-date"}, 1_000_000.0) is False


def test_inside_leg_is_recent_false_for_future_stamp():
    """codex P3: a future/corrupt stamp must not count as recent."""
    from fno.agents.dispatch import _inside_leg_is_recent

    now = 1_000_000.0
    stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now + 300))
    assert _inside_leg_is_recent({"received_at": stamp}, now) is False
