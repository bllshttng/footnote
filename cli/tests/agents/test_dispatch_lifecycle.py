"""Tests for fno.agents.dispatch lifecycle verbs.

Wave-1 coverage (US4-lifecycle):

- ``stop_agent`` (AC1-* in the design doc)
- ``rm_agent`` (AC2-*)
- ``reconcile_agents`` (AC3-*)
- ``attach_agent`` (AC7-*)

Each test monkeypatches the corresponding helper in
``fno.agents.providers.{claude,codex}`` so we exercise the
dispatch surface in isolation; the provider adapters have their own
shellout-level tests.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


def _seed_registry(*entries):
    """Persist a list of AgentEntry dicts as the active registry."""
    from fno.agents.registry import AgentEntry, write_registry

    out: list[AgentEntry] = []
    for kwargs in entries:
        kwargs.setdefault("cwd", "/tmp")
        kwargs.setdefault("log_path", "/tmp/x.log")
        _canonize_identity_kwargs(kwargs)
        out.append(AgentEntry(**kwargs))
    write_registry(out)
    return out


def _canonize_identity_kwargs(kwargs: dict) -> None:
    """Map legacy identity kwargs to the v10 canonical fields (x-880e), mirroring
    what load_registry back-fills for on-disk rows, so terse test dicts still build."""
    if "provider" in kwargs:
        kwargs["harness"] = kwargs.pop("provider")
    for _k in ("codex_session_id", "gemini_session_id", "claude_session_uuid"):
        if _k in kwargs:
            kwargs.setdefault("harness_session_id", kwargs.pop(_k))


def _force_claude_on_path(monkeypatch, tmp_path: Path) -> None:
    """Make ``is_provider_available('claude')`` return True without a real binary.

    We monkeypatch ``shutil.which`` via the ``dispatch`` module so all
    PATH checks see a positive result; the per-call ``claude_*`` helpers
    are independently patched per-test.
    """
    from fno.agents import dispatch as dispatch_mod

    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir(exist_ok=True)
    fake = bin_dir / "claude"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))
    # Defense against test order: the dispatch module imports shutil at
    # import time; PATH env tweak above is sufficient because
    # shutil.which is called on every is_provider_available invocation.
    assert dispatch_mod.is_provider_available("claude") is True


def _read_events(tmp_path: Path) -> list[dict]:
    """Return all events.jsonl records (or empty if file absent).

    Mirrors the helper in test_codex_fatal_error_dispatch.py; copied
    here so each lifecycle test can verify the forensic event-stream
    contract independently of provider failure tests.
    """
    from fno import paths

    events_path = paths.state_dir() / "events.jsonl"
    if not events_path.exists():
        return []
    out: list[dict] = []
    for line in events_path.read_text().splitlines():
        if not line.strip():
            continue
        out.append(json.loads(line))
    return out


# ---------------------------------------------------------------------------
# stop_agent — AC1-*
# ---------------------------------------------------------------------------


def test_stop_claude_happy_path(tmp_path: Path, monkeypatch, capsys) -> None:
    """AC1-HP: claude stop succeeds, emits agent_stopped, prints summary."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    calls: list[tuple[str, float]] = []

    def fake_claude_stop(short_id: str, *, timeout: float = 30.0):
        calls.append((short_id, timeout))
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_stop", fake_claude_stop)

    result = dispatch.stop_agent("worker-claude")

    assert result.name == "worker-claude"
    assert result.provider == "claude"
    assert result.claude_exit == 0
    assert calls == [("7c5dcf5d", 30.0)]

    out = capsys.readouterr().out
    assert "stopped: worker-claude (7c5dcf5d)" in out

    # AC1-HP forensic contract: agent_stopped event carries claude_exit=0,
    # provider=claude, and the short_id for downstream audit.
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0]["provider"] == "claude"
    assert stop_events[0]["claude_exit"] == 0
    assert stop_events[0]["short_id"] == "7c5dcf5d"


def test_stop_claude_nonzero_exit_propagates(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC1-ERR: claude stop non-zero passes stderr through and exit_code=1."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod,
        "claude_stop",
        lambda short_id, *, timeout=30.0: (
            5,
            "claude stop: session already stopped\n",
        ),
    )

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.stop_agent("worker-claude")

    assert exc_info.value.exit_code == 1
    err = capsys.readouterr().err
    assert "session already stopped" in err

    # AC1-ERR forensic contract: agent_stopped event must carry the
    # non-zero claude_exit even when the operator-facing flow raises.
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0]["claude_exit"] == 5


def test_stop_agent_not_found(tmp_path: Path, monkeypatch) -> None:
    """AC1-UI: stop on a non-existent name exits 2 without spawning subprocess."""
    use_tmpdir(monkeypatch, tmp_path)
    # No registry entry seeded.

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    spawn_called = False

    def fake_stop(short_id, *, timeout=30.0):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_stop", fake_stop)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.stop_agent("ghost")

    assert exc_info.value.exit_code == 2
    assert "not found in registry" in str(exc_info.value)
    assert spawn_called is False


@pytest.mark.parametrize("verb", ["stop", "rm", "attach"])
def test_lifecycle_verbs_refuse_unavailable_identity_evidence(
    tmp_path: Path,
    monkeypatch,
    verb: str,
) -> None:
    """Unreadable identity stores cannot degrade into exact-name selection."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="victim", provider="claude", short_id="7c5dcf5d"),
    )

    from fno.agents import dispatch
    from fno.agents import registry as registry_mod
    from fno.agents.providers import claude as claude_mod

    def unavailable(*_args, **_kwargs):
        raise registry_mod.AgentResolutionError(
            "identity evidence unavailable",
            unavailable=True,
        )

    monkeypatch.setattr(registry_mod, "resolve_agent", unavailable)
    shellouts: list[str] = []
    monkeypatch.setattr(
        claude_mod,
        "claude_stop",
        lambda *_args, **_kwargs: shellouts.append("stop") or (0, ""),
    )
    monkeypatch.setattr(
        claude_mod,
        "claude_rm",
        lambda *_args, **_kwargs: shellouts.append("rm") or (0, ""),
    )
    monkeypatch.setattr(
        claude_mod,
        "claude_attach",
        lambda *_args, **_kwargs: shellouts.append("attach") or 0,
    )

    action = {
        "stop": dispatch.stop_agent,
        "rm": dispatch.rm_agent,
        "attach": dispatch.attach_agent,
    }[verb]
    with pytest.raises(dispatch.DispatchAskError, match="identity evidence unavailable") as exc:
        action("victim")

    assert exc.value.exit_code == 12
    assert shellouts == []
    assert [entry.name for entry in registry_mod.load_registry()] == ["victim"]


@pytest.mark.parametrize("verb", ["stop", "rm"])
def test_destructive_lifecycle_refuses_duplicate_registry_name_after_full_id(
    tmp_path: Path,
    monkeypatch,
    verb: str,
) -> None:
    """A full id cannot be collapsed back to a corrupt shared registry name."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import dispatch
    from fno.agents import registry as registry_mod
    from fno.agents.providers import claude as claude_mod

    first = registry_mod.AgentEntry(
        name="same",
        harness="claude",
        harness_session_id="aaaaaaaa-1111-7222-8333-4444deadbeef",
        short_id="transport1",
        cwd="/one",
        log_path="/tmp/one.log",
    )
    second = registry_mod.AgentEntry(
        name="same",
        harness="claude",
        harness_session_id="bbbbbbbb-1111-7222-8333-4444cafefeed",
        short_id="transport2",
        cwd="/two",
        log_path="/tmp/two.log",
    )
    rows = [first, second]
    monkeypatch.setattr(registry_mod, "load_registry", lambda *_a, **_k: rows)
    monkeypatch.setattr(dispatch, "load_registry", lambda *_a, **_k: rows)
    shellouts: list[str] = []
    monkeypatch.setattr(
        claude_mod,
        "claude_stop",
        lambda short_id, **_kwargs: shellouts.append(short_id) or (0, ""),
    )
    monkeypatch.setattr(
        claude_mod,
        "claude_rm",
        lambda short_id, **_kwargs: shellouts.append(short_id) or (0, ""),
    )

    action = dispatch.stop_agent if verb == "stop" else dispatch.rm_agent
    with pytest.raises(dispatch.DispatchAskError, match="ambiguous") as exc:
        action(second.harness_session_id)

    assert exc.value.exit_code == 2
    assert shellouts == []


@pytest.mark.parametrize("verb", ["stop", "rm"])
def test_destructive_lifecycle_pins_full_id_across_name_lock(
    tmp_path: Path,
    monkeypatch,
    verb: str,
) -> None:
    """A same-name replacement cannot inherit a full-id lifecycle request."""
    use_tmpdir(monkeypatch, tmp_path)
    from contextlib import contextmanager
    from fno.agents import dispatch
    from fno.agents import registry as registry_mod
    from fno.agents.providers import claude as claude_mod

    original = registry_mod.AgentEntry(
        name="victim",
        harness="claude",
        harness_session_id="aaaaaaaa-1111-7222-8333-4444deadbeef",
        short_id="transportA",
        cwd="/one",
        log_path="/tmp/one.log",
        created_at="2026-07-30T10:00:00Z",
    )
    replacement = registry_mod.AgentEntry(
        name="victim",
        harness="claude",
        harness_session_id="bbbbbbbb-1111-7222-8333-4444cafefeed",
        short_id="transportB",
        cwd="/two",
        log_path="/tmp/two.log",
        created_at="2026-07-30T10:00:01Z",
    )
    monkeypatch.setattr(
        registry_mod,
        "resolve_agent",
        lambda *_a, **_k: registry_mod.ResolvedAgent(
            entry=original,
            matched_by="full_session_id",
        ),
    )
    reads = {"count": 0}

    def staged_read(*_args, **_kwargs):
        reads["count"] += 1
        return original if reads["count"] == 1 else replacement

    @contextmanager
    def unlocked(*_args, **_kwargs):
        yield object()

    monkeypatch.setattr(dispatch, "_resolve_registry_entry", staged_read)
    monkeypatch.setattr(dispatch, "hold_agent_lock", unlocked)
    shellouts: list[str] = []
    monkeypatch.setattr(
        claude_mod,
        "claude_stop",
        lambda short_id, **_kwargs: shellouts.append(short_id) or (0, ""),
    )
    monkeypatch.setattr(
        claude_mod,
        "claude_rm",
        lambda short_id, **_kwargs: shellouts.append(short_id) or (0, ""),
    )

    action = dispatch.stop_agent if verb == "stop" else dispatch.rm_agent
    with pytest.raises(dispatch.DispatchAskError, match="recipient identity changed"):
        action(original.harness_session_id)

    assert reads["count"] == 2
    assert shellouts == []


@pytest.mark.parametrize("address_by_name", [False, True])
def test_rm_retains_row_restamped_during_shellout(
    tmp_path: Path,
    monkeypatch,
    address_by_name: bool,
) -> None:
    """A restamp after rm's side effect cannot make a replacement inherit deletion."""
    use_tmpdir(monkeypatch, tmp_path)
    original_id = "aaaaaaaa-1111-7222-8333-4444deadbeef"
    replacement_id = "bbbbbbbb-1111-7222-8333-4444cafefeed"
    _seed_registry(
        dict(
            name="victim",
            provider="claude",
            harness_session_id=original_id,
            short_id="transportA",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents import registry as registry_mod
    from fno.agents.providers import claude as claude_mod

    def restamp_during_rm(*_args, **_kwargs):
        registry_mod.restamp_harness_session_id(
            name="victim",
            harness="claude",
            session_id=replacement_id,
        )
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", restamp_during_rm)

    with pytest.raises(dispatch.DispatchAskError, match="identity changed during rm") as exc:
        dispatch.rm_agent("victim" if address_by_name else original_id)

    assert exc.value.exit_code == 12
    rows = registry_mod.load_registry()
    assert len(rows) == 1
    assert rows[0].harness_session_id == replacement_id


@pytest.mark.parametrize("address_by_name", [False, True])
def test_stop_does_not_stamp_row_restamped_during_shellout(
    tmp_path: Path,
    monkeypatch,
    address_by_name: bool,
) -> None:
    """A successful stop reports success without stamping a replacement row."""
    use_tmpdir(monkeypatch, tmp_path)
    original_id = "aaaaaaaa-1111-7222-8333-4444deadbeef"
    replacement_id = "bbbbbbbb-1111-7222-8333-4444cafefeed"
    _seed_registry(
        dict(
            name="victim",
            provider="claude",
            harness_session_id=original_id,
            short_id="transportA",
            status="live",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents import registry as registry_mod
    from fno.agents.providers import claude as claude_mod

    def restamp_during_stop(*_args, **_kwargs):
        registry_mod.restamp_harness_session_id(
            name="victim",
            harness="claude",
            session_id=replacement_id,
        )
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_stop", restamp_during_stop)

    result = dispatch.stop_agent("victim" if address_by_name else original_id)

    assert result.claude_exit == 0
    rows = registry_mod.load_registry()
    assert len(rows) == 1
    assert rows[0].harness_session_id == replacement_id
    assert rows[0].status == "live"
    assert any(
        event.get("kind") == "agent_stopped_status_write_failed"
        and event.get("reason") == "recipient_identity_changed"
        for event in _read_events(tmp_path)
    )


@pytest.mark.parametrize("verb", ["stop", "rm"])
def test_lifecycle_does_not_mutate_duplicate_name_rows_added_during_shellout(
    tmp_path: Path,
    monkeypatch,
    verb: str,
) -> None:
    """A newly ambiguous name cannot inherit a selected row's lifecycle write."""
    use_tmpdir(monkeypatch, tmp_path)
    original = _seed_registry(
        dict(
            name="victim",
            provider="claude",
            harness_session_id="aaaaaaaa-1111-7222-8333-4444deadbeef",
            short_id="transportA",
            status="live",
        ),
    )[0]
    _force_claude_on_path(monkeypatch, tmp_path)

    from dataclasses import replace
    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    replacement = replace(
        original,
        harness_session_id="bbbbbbbb-1111-7222-8333-4444cafefeed",
        short_id="transportB",
        created_at="2026-07-30T10:00:01Z",
    )
    persisted: list = []

    def update_with_duplicate(updater):
        persisted[:] = updater([original, replacement])
        return persisted

    monkeypatch.setattr(dispatch, "update_registry", update_with_duplicate)
    monkeypatch.setattr(claude_mod, "claude_stop", lambda *_a, **_k: (0, ""))
    monkeypatch.setattr(claude_mod, "claude_rm", lambda *_a, **_k: (0, ""))

    if verb == "rm":
        with pytest.raises(dispatch.DispatchAskError, match="identity changed during rm"):
            dispatch.rm_agent("victim")
    else:
        assert dispatch.stop_agent("victim").claude_exit == 0

    assert len(persisted) == 2
    assert {entry.status for entry in persisted} == {"live"}


def test_stop_codex_is_no_op(tmp_path: Path, monkeypatch, capsys) -> None:
    """AC1-EDGE: codex agents print info on stderr and exit 0; no subprocess."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    spawn_called = False

    def fake_stop(*args, **kwargs):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_stop", fake_stop)

    result = dispatch.stop_agent("worker-codex")

    assert result.provider == "codex"
    assert result.claude_exit is None
    assert spawn_called is False
    err = capsys.readouterr().err
    assert "codex agents are synchronous" in err

    # AC1-EDGE forensic contract: codex stop emits the same event kind so
    # an external observer can count stop activity uniformly.
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0]["provider"] == "codex"
    assert stop_events[0]["claude_exit"] is None


def test_stop_claude_timeout_maps_to_exit_15(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-FR: shellout timeout raises DispatchAskError(exit_code=15)."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    def fake_stop(short_id, *, timeout=30.0):
        raise subprocess.TimeoutExpired(cmd=["claude", "stop", short_id], timeout=timeout)

    monkeypatch.setattr(claude_mod, "claude_stop", fake_stop)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.stop_agent("worker-claude")

    assert exc_info.value.exit_code == 15
    assert "timed out" in str(exc_info.value)

    # AC1-FR forensic contract: timeout path emits with timed_out=true so
    # observers can distinguish "shellout exit non-zero" from "shellout
    # never returned".
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0].get("timed_out") is True
    assert stop_events[0]["claude_exit"] is None


# ---------------------------------------------------------------------------
# stop_agent — transport-id fallback chain (x-a4b2)
# ---------------------------------------------------------------------------


def _spawn_sleeper():
    """Start a real, harmless child process and return (popen, start_token).

    These tests signal a genuine process rather than asserting on a mocked
    ``os.kill``: the defect they cover is a live worker surviving ``stop``,
    so the assertion has to be that the process actually died.
    """
    import sys

    from fno.agents.spawn_gate import _process_start_time

    proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    return proc, _process_start_time(proc.pid)


def test_stop_kills_pid_when_no_transport_id(tmp_path: Path, monkeypatch, capsys) -> None:
    """A row with a live pid and no transport id is stopped, not refused.

    The duplicate-worker half of the wave-boundary handoff failure: the
    orphan had a pid and no short_id/session_id, so `stop` refused and it
    had to be killed by hand to restore one-writer semantics.
    """
    use_tmpdir(monkeypatch, tmp_path)
    proc, start_token = _spawn_sleeper()
    try:
        _seed_registry(
            dict(
                name="orphan",
                provider="claude",
                short_id="",
                pid=proc.pid,
                pid_start_time=start_token,
            ),
        )

        from fno.agents import dispatch
        from fno.agents.registry import load_registry

        result = dispatch.stop_agent("orphan")

        assert result.name == "orphan"
        # The process is really gone, not merely reported stopped.
        assert proc.wait(timeout=10) is not None
        assert "stopped: orphan (pid " in capsys.readouterr().out
        assert load_registry()[0].status == "orphaned"

        stop_events = [e for e in _read_events(tmp_path) if e.get("kind") == "agent_stopped"]
        assert len(stop_events) == 1
        assert stop_events[0]["stopped_by"] == "pid"
    finally:
        proc.kill()
        proc.wait()


def test_stop_refuses_pid_without_start_token(tmp_path: Path, monkeypatch) -> None:
    """Bare liveness is not enough to justify a SIGKILL.

    Without the recorded start token there is no way to tell our worker from an
    unrelated process that inherited the pid after it died, so the row is
    refused and the process is left alone.
    """
    use_tmpdir(monkeypatch, tmp_path)
    proc, _ = _spawn_sleeper()
    try:
        _seed_registry(
            dict(name="tokenless", provider="claude", short_id="", pid=proc.pid),
        )

        from fno.agents import dispatch

        with pytest.raises(dispatch.DispatchAskError) as exc_info:
            dispatch.stop_agent("tokenless")

        assert exc_info.value.exit_code == 12
        assert proc.poll() is None, "a refused row must not be signalled"
    finally:
        proc.kill()
        proc.wait()


def test_stop_refuses_recycled_pid(tmp_path: Path, monkeypatch) -> None:
    """A pid whose start token no longer matches belongs to someone else.

    Signalling it would kill an unrelated process, so the row is refused
    and the live process must survive untouched.
    """
    use_tmpdir(monkeypatch, tmp_path)
    proc, start_token = _spawn_sleeper()
    try:
        _seed_registry(
            dict(
                name="recycled",
                provider="claude",
                short_id="",
                pid=proc.pid,
                pid_start_time=(start_token or 0) + 1,
            ),
        )

        from fno.agents import dispatch

        with pytest.raises(dispatch.DispatchAskError) as exc_info:
            dispatch.stop_agent("recycled")

        assert exc_info.value.exit_code == 12
        # The refusal must not point at `rm`, which clears the row and
        # leaves the process running.
        assert "does NOT stop" in str(exc_info.value)
        assert proc.poll() is None, "unrelated process must not be signalled"
    finally:
        proc.kill()
        proc.wait()


# ---------------------------------------------------------------------------
# rm_agent — AC2-*
# ---------------------------------------------------------------------------


def test_rm_claude_happy_path(tmp_path: Path, monkeypatch, capsys) -> None:
    """AC2-HP: claude rm exits 0, registry row removed."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (0, ""),
    )

    result = dispatch.rm_agent("worker-claude")

    assert result.registry_changed is True
    assert result.claude_exit == 0
    assert load_registry() == []
    assert "removed: worker-claude" in capsys.readouterr().out

    # AC2-HP forensic contract: agent_removed event with claude_exit=0,
    # force=false, registry_changed=true.
    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0]["claude_exit"] == 0
    assert rm_events[0]["force"] is False
    assert rm_events[0]["registry_changed"] is True


def test_rm_claude_refusal_leaves_registry_unchanged(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC2-ERR: non-forceful claude refusal -> stderr passthrough, registry unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (
            1,
            "session has uncommitted changes; commit or stash first\n",
        ),
    )

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 1
    # Registry preserved.
    entries = load_registry()
    assert len(entries) == 1
    assert entries[0].name == "worker-claude"
    err = capsys.readouterr().err
    assert "uncommitted changes" in err

    # AC2-ERR forensic contract: refusal event with registry_changed=false
    # is what external `fno agents list` vs claude-supervisor diff
    # reconciliation depends on. Drop this emit and the chain breaks
    # silently.
    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0]["claude_exit"] == 1
    assert rm_events[0]["force"] is False
    assert rm_events[0]["registry_changed"] is False


def test_rm_force_overrides_claude_refusal(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC2-UI: --force removes the registry row even when claude rm fails."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (
            1,
            "session has uncommitted changes\n",
        ),
    )

    result = dispatch.rm_agent("worker-claude", force=True)

    assert result.force is True
    assert result.claude_exit == 1
    assert result.registry_changed is True
    assert load_registry() == []
    err = capsys.readouterr().err
    assert "WARN: claude rm failed but --force given" in err

    # AC2-UI: --force override emits with both claude_exit (preserved) and
    # registry_changed=true so post-hoc forensics can see "operator chose
    # to drop the row despite claude's refusal".
    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0]["claude_exit"] == 1
    assert rm_events[0]["force"] is True
    assert rm_events[0]["registry_changed"] is True


def test_rm_codex_is_registry_only(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC2-EDGE: codex rm removes registry row; no subprocess spawned."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    spawn_called = False

    def fake_rm(*args, **kwargs):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    result = dispatch.rm_agent("worker-codex")

    assert spawn_called is False
    assert load_registry() == []
    assert result.registry_changed is True
    assert "removed: worker-codex" in capsys.readouterr().out


def test_rm_claude_not_on_path(tmp_path: Path, monkeypatch) -> None:
    """AC2-FR: claude not on PATH exits 14, registry unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    monkeypatch.setenv("PATH", "/nonexistent")

    from fno.agents import dispatch
    from fno.agents.registry import load_registry

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 14
    entries = load_registry()
    assert len(entries) == 1  # registry untouched


# ---------------------------------------------------------------------------
# reconcile_agents — AC3-*
# ---------------------------------------------------------------------------


def test_reconcile_orphan_detection(tmp_path: Path, monkeypatch) -> None:
    """AC3-HP: live claude agent flips to orphaned when logs probe fails."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            status="live",
            last_message_at="2026-05-01T00:00:00Z",
        ),
    )
    # is_provider_available("claude") must return True so reconcile reaches
    # the claude_logs_reachable monkeypatch instead of routing to errors
    # (sigma-review C1 fix: missing claude -> errors, not orphaned).
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable",
        lambda short_id, *, timeout=10.0: False,
    )

    result = dispatch.reconcile_agents()

    assert result.scanned == 1
    assert len(result.orphaned) == 1
    assert result.orphaned[0]["name"] == "worker-claude"
    # last_message_at preserved.
    entries = load_registry()
    assert entries[0].status == "orphaned"
    assert entries[0].last_message_at == "2026-05-01T00:00:00Z"

    # AC3-HP forensic contract: reconcile_done aggregate with the right counts.
    events = _read_events(tmp_path)
    done = [e for e in events if e.get("kind") == "reconcile_done"]
    assert len(done) == 1
    assert done[0]["scanned"] == 1
    assert done[0]["orphaned"] == 1
    assert done[0]["recovered"] == 0


def test_reconcile_recovery(tmp_path: Path, monkeypatch) -> None:
    """AC3-ERR: orphaned claude agent flips back to live on reachable probe."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            status="orphaned",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable",
        lambda short_id, *, timeout=10.0: True,
    )

    result = dispatch.reconcile_agents()

    assert len(result.recovered) == 1
    assert load_registry()[0].status == "live"

    # AC3-ERR forensic contract.
    events = _read_events(tmp_path)
    done = [e for e in events if e.get("kind") == "reconcile_done"]
    assert len(done) == 1
    assert done[0]["recovered"] == 1
    assert done[0]["orphaned"] == 0


def test_reconcile_backfills_null_harness_session_id(tmp_path: Path, monkeypatch) -> None:
    """AC1-FR / AC1-UI (x-ec59): a live claude row whose canonical id never landed
    (the spawn-time uuid resolution raced) is healed from the harness store, and
    reconcile names the backfilled row in its result + the reconcile_done event."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            status="live",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable", lambda short_id, *, timeout=10.0: True
    )
    monkeypatch.setattr(
        claude_mod, "resolve_session_uuid", lambda short_id: "FULL-UUID-7c5dcf5d"
    )

    result = dispatch.reconcile_agents()

    assert len(result.backfilled) == 1
    assert result.backfilled[0]["name"] == "worker-claude"
    assert result.backfilled[0]["harness_session_id"] == "FULL-UUID-7c5dcf5d"

    # The row gains the canonical harness_session_id field.
    e = load_registry()[0]
    assert e.harness_session_id == "FULL-UUID-7c5dcf5d"

    events = _read_events(tmp_path)
    done = [ev for ev in events if ev.get("kind") == "reconcile_done"]
    assert done and done[0]["backfilled"] == 1


def test_reconcile_backfill_empty_when_id_already_present(tmp_path: Path, monkeypatch) -> None:
    """AC1-UI: a live row that already carries its canonical id yields an empty
    backfill set (so "nothing to heal" is distinguishable from "healed"), and the
    resolver is never even consulted."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            harness="claude",
            harness_session_id="ALREADY",
            status="live",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable", lambda short_id, *, timeout=10.0: True
    )

    def _must_not_resolve(short_id):
        raise AssertionError("resolve_session_uuid called for an already-healed row")

    monkeypatch.setattr(claude_mod, "resolve_session_uuid", _must_not_resolve)

    result = dispatch.reconcile_agents()
    assert result.backfilled == []


def test_reconcile_json_shape_round_trips_gemini_error(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-UI: a legacy Gemini row is JSON-safe and explicitly skipped."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-gemini",
            provider="gemini",
            gemini_session_id="some-id",  # too short for the 8-hex prefix probe
        ),
    )

    from fno.agents import dispatch

    result = dispatch.reconcile_agents()
    assert result.scanned == 1
    assert len(result.errors) == 1
    assert result.errors[0]["provider"] == "gemini"
    assert result.errors[0]["reason"] == "retired-provider"
    # Round-trips through json without error.
    payload = {
        "scanned": result.scanned,
        "orphaned": result.orphaned,
        "recovered": result.recovered,
        "skipped": result.skipped,
        "errors": result.errors,
    }
    encoded = json.dumps(payload)
    decoded = json.loads(encoded)
    assert decoded["errors"][0]["provider"] == "gemini"


def test_reconcile_codex_index_missing(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC3-EDGE: missing ~/.codex/session_index.jsonl yields an error entry,
    codex statuses untouched, exit 0."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    # Override the index path to a non-existent file.
    missing = tmp_path / "no-codex-index.jsonl"
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: missing
    )

    result = dispatch.reconcile_agents()

    assert len(result.errors) == 1
    assert result.errors[0]["reason"] == "codex-session-index-missing"
    # Status unchanged.
    assert load_registry()[0].status == "live"
    err = capsys.readouterr().err
    assert "codex session index missing" in err


def test_reconcile_codex_reachable_flips_to_live(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-FR companion: codex session listed in the index flips orphaned -> live."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id=session_id,
            status="orphaned",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    index = tmp_path / "session_index.jsonl"
    index.write_text(json.dumps({"session_id": session_id}) + "\n")
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: index
    )

    result = dispatch.reconcile_agents()

    assert len(result.recovered) == 1
    assert load_registry()[0].status == "live"


def test_reconcile_backfills_late_codex_pane_identity_without_index(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-CON: the live pane PID is sufficient authority for late binding."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            status="spawning",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch
    from fno.agents import mux_spawn
    from fno.agents import spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, started: pid == 4242)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(
        mux_spawn,
        "_codex_session_id_for_pid",
        lambda pid: session_id if pid == 4242 else None,
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    row = load_registry()[0]
    assert row.harness_session_id == session_id
    assert row.status == "live"
    assert result.backfilled == [
        {
            "name": "worker-codex",
            "provider": "codex",
            "harness_session_id": session_id,
        }
    ]


def test_reconcile_orphans_a_dead_idless_claude_pane(
    tmp_path: Path, monkeypatch
) -> None:
    """A happy-hosted claude pane has no session id until its worker restamps.

    Before this arm existed the row fell to `missing-claude-short-id` (a mux row's
    short_id is deliberately empty) and was never orphaned, so a dead pane held
    its name against every future spawn of that name.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-happy",
            provider="claude",
            status="spawning",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: False)
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, started: False)

    dispatch.reconcile_agents()

    assert load_registry()[0].status == "orphaned"


def test_reconcile_orphans_a_dead_claude_pane_that_already_restamped(
    tmp_path: Path, monkeypatch
) -> None:
    """The pane is the oracle for a pane row's whole life, not just before it has
    an id. Gating the arm on a missing id covered the row only until its worker
    restamped, and a pane that died after that was reachable by no arm at all."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-happy",
            provider="claude",
            status="live",
            harness_session_id="019fb024-2327-75f3-8b80-06e9d5ade05f",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: False)
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, started: False)

    dispatch.reconcile_agents()

    assert load_registry()[0].status == "orphaned"


def test_reconcile_correlates_the_pane_child_even_with_an_incarnation_token(
    tmp_path: Path, monkeypatch
) -> None:
    """`pid_start_time` proves the stored pid's incarnation is alive, never which
    pane it now belongs to. After a mux restart recycles (session, pane_id) the
    original child can still be running while the pane is someone else's, and pane
    liveness and pid liveness are then both true about different processes."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-happy",
            provider="claude",
            status="live",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, started: True)
    # The pane now hosts a different child than the one this row recorded.
    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *_a: 9999)

    result = dispatch.reconcile_agents()

    assert load_registry()[0].status == "live", "preserved, never asserted"
    assert any(e["reason"] == "claude-pane-pid-unconfirmed" for e in result.errors)


def test_reconcile_never_trusts_an_uncorrelated_claude_pane_pid(
    tmp_path: Path, monkeypatch
) -> None:
    """A legacy row carries no incarnation token, so `_pid_alive` degrades to bare
    existence. A mux restart can hand the same (session, pane_id) to a different
    child while the recorded pid is recycled and alive; trusting that keeps the
    row live and points name-based delivery at a stranger."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-happy",
            provider="claude",
            status="live",
            pid=4242,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    # The pane's real child is someone else; the recorded pid is a recycled one
    # that happens to still exist.
    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *_a: 9999)
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, started: True)

    result = dispatch.reconcile_agents()

    assert load_registry()[0].status == "live", "status is preserved, not asserted"
    assert any(e["reason"] == "claude-pane-pid-unconfirmed" for e in result.errors)


def test_reconcile_keeps_a_live_claude_pane_with_no_pid_pending(
    tmp_path: Path, monkeypatch
) -> None:
    """Missing pid evidence is inconclusive, never dead. `orphaned` is terminal
    and a later restamp only promotes `spawning`, so orphaning a live pane on an
    absent pid would strand a healthy worker permanently."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-happy",
            provider="claude",
            status="spawning",
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *_a: None)

    result = dispatch.reconcile_agents()

    assert load_registry()[0].status == "spawning"
    assert any(e["reason"] == "claude-pane-pid-pending" for e in result.errors)


def test_reconcile_keeps_a_live_idless_claude_pane_waiting(
    tmp_path: Path, monkeypatch
) -> None:
    """Still alive and still owed its restamp: waiting is correct, not an error."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-happy",
            provider="claude",
            status="spawning",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, started: pid == 4242)

    result = dispatch.reconcile_agents()

    assert load_registry()[0].status == "spawning"
    assert not any(e["reason"] == "missing-claude-short-id" for e in result.errors)


def test_reconcile_keeps_live_unresolved_codex_pane_pending(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-FR: a live PID with no rollout stays pending, never orphaned."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            status="spawning",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _started: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(mux_spawn, "_codex_session_id_for_pid", lambda _pid: None)

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    row = load_registry()[0]
    assert row.status == "spawning"
    assert row.harness_session_id is None
    assert result.orphaned == []
    assert result.backfilled == []
    assert result.errors[0]["reason"] == "codex-session-id-pending"


def test_reconcile_refuses_duplicate_late_codex_identity(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-CON: two registry rows may never share one full Codex thread ID."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(
            name="owner",
            provider="codex",
            harness_session_id=session_id,
            status="live",
        ),
        dict(
            name="late",
            provider="codex",
            status="spawning",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _started: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(
        mux_spawn, "_codex_session_id_for_pid", lambda _pid: session_id
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    late = next(row for row in load_registry() if row.name == "late")
    assert late.harness_session_id is None
    assert late.status == "spawning"
    assert result.backfilled == []
    assert any(e["reason"] == "duplicate-codex-session-id" for e in result.errors)


def test_reconcile_refuses_duplicate_across_two_rows_in_one_pass(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-CON: two id-less rows resolving to ONE rollout cannot both take it.

    Both rows read the other as ``harness_session_id=None``, so a duplicate check
    that only scans stored rows clears both and stamps the identity twice - the
    exact collision the guard exists to prevent.
    """
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(name="late-a", provider="codex", status="spawning", pid=4242,
             pid_start_time=123, mux={"session": "main", "pane_id": 7}),
        dict(name="late-b", provider="codex", status="spawning", pid=4242,
             pid_start_time=123, mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _started: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(
        mux_spawn, "_codex_session_id_for_pid", lambda _pid: session_id
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    rows = {row.name: row for row in load_registry()}
    bound = [n for n, r in rows.items() if r.harness_session_id == session_id]
    assert len(bound) <= 1, f"one rollout stamped onto {bound}"
    loser = next(n for n in ("late-a", "late-b") if n not in bound)
    assert rows[loser].harness_session_id is None
    assert rows[loser].status == "spawning"
    assert len(result.backfilled) == len(bound)


def test_reconcile_refuses_unconfirmed_legacy_pid(
    tmp_path: Path, monkeypatch
) -> None:
    """A recorded pid with no incarnation token is never trusted on its own.

    Legacy rows (written before pid_start_time stamping) are the population this
    heal repairs, and for them ``_pid_alive`` degrades to bare existence - so a
    recycled pid could bind a stranger's rollout. The pane's real child must agree.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="legacy", provider="codex", status="spawning", pid=4242,
             mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _started: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    # The pane's actual child is a DIFFERENT pid: 4242 was recycled.
    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *_a, **_k: 9999)
    monkeypatch.setattr(
        mux_spawn,
        "_codex_session_id_for_pid",
        lambda _pid: "019fbfff-dead-7000-8000-000000000000",
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    row = load_registry()[0]
    assert row.harness_session_id is None, "a stranger's rollout was stamped"
    assert result.backfilled == []
    assert any(e["reason"] == "codex-pane-pid-unconfirmed" for e in result.errors)


def test_reconcile_heals_legacy_pid_when_the_pane_confirms_it(
    tmp_path: Path, monkeypatch
) -> None:
    """Positive control for the check above: agreement heals, so the refusal
    above is proving the mismatch and not merely that legacy rows never bind."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(name="legacy", provider="codex", status="spawning", pid=4242,
             mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _started: True)
    monkeypatch.setattr(spawn_gate, "_process_start_time", lambda _pid: 777)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *_a, **_k: 4242)
    monkeypatch.setattr(
        mux_spawn, "_codex_session_id_for_pid", lambda _pid: session_id
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    row = load_registry()[0]
    assert row.harness_session_id == session_id
    assert row.status == "live"
    assert row.pid_start_time == 777, "the heal stamps the incarnation token"
    assert len(result.backfilled) == 1


def test_reconcile_normalizes_under_lock_duplicate_to_spawning(
    tmp_path: Path, monkeypatch
) -> None:
    """A rollout claimed after probing still cannot leave the loser live."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(name="late", provider="codex", status="live", pid=4242,
             pid_start_time=123, mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import AgentEntry, load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_args, **_kwargs: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(mux_spawn, "_codex_session_id_for_pid", lambda _pid: session_id)
    real_update = dispatch.update_registry
    injected = False

    def claim_then_apply(updater):
        nonlocal injected
        if not injected:
            injected = True
            real_update(lambda rows: rows + [AgentEntry(
                name="owner", harness="codex", cwd="/tmp", log_path="",
                harness_session_id=session_id, status="live",
            )])
        return real_update(updater)

    monkeypatch.setattr(dispatch, "update_registry", claim_then_apply)
    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    late = next(row for row in load_registry() if row.name == "late")
    assert late.status == "spawning"
    assert late.harness_session_id is None
    assert any(e["reason"] == "codex-session-id-backfill-raced" for e in result.errors)


def test_reconcile_never_claims_a_refused_claude_backfill(
    tmp_path: Path, monkeypatch
) -> None:
    """`backfilled` reports what the write STAMPED, not what it intended.

    The under-lock guard refuses a claude row whose short_id changed between probe
    and apply (rm + re-register under the same name). Claiming the heal at queue
    time reported a bound identity for a row whose harness_session_id is still
    null - the same lie this PR removes from the codex path.
    """
    use_tmpdir(monkeypatch, tmp_path)
    healed_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    _seed_registry(
        dict(name="w1", provider="claude", status="live", short_id="11111111"),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import AgentEntry, load_registry

    monkeypatch.setattr(dispatch, "is_provider_available", lambda _p: True)
    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable", lambda *_a, **_k: True
    )
    monkeypatch.setattr(claude_mod, "resolve_session_uuid", lambda _s: healed_uuid)

    real_update = dispatch.update_registry
    replaced = False

    def replace_then_apply(updater):
        nonlocal replaced
        if not replaced:
            replaced = True
            # Same name, different transport handle: the row we probed is gone.
            real_update(lambda rows: [
                AgentEntry(
                    name="w1", harness="claude", cwd="/tmp", log_path="",
                    short_id="99999999", status="live",
                )
            ])
        return real_update(updater)

    monkeypatch.setattr(dispatch, "update_registry", replace_then_apply)
    result = dispatch.reconcile_agents()

    row = next(r for r in load_registry() if r.name == "w1")
    assert row.harness_session_id is None, "a refused stamp still landed"
    assert not any(
        b["name"] == "w1" for b in result.backfilled
    ), f"claimed a heal that was refused: {result.backfilled}"
    assert any(
        e["reason"] == "claude-session-id-backfill-raced" for e in result.errors
    )


def test_reconcile_normalizes_duplicate_idless_live_row_to_spawning(
    tmp_path: Path, monkeypatch
) -> None:
    """A duplicate rollout never leaves an identity-less row falsely live."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(name="owner", provider="codex", harness_session_id=session_id, status="live"),
        dict(name="late", provider="codex", status="live", pid=4242,
             pid_start_time=123, mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_args, **_kwargs: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args, **_kwargs: True)
    monkeypatch.setattr(mux_spawn, "_codex_session_id_for_pid", lambda _pid: session_id)

    dispatch.reconcile_agents(codex_session_index_path=tmp_path / "missing-index.jsonl")

    late = next(row for row in load_registry() if row.name == "late")
    assert late.status == "spawning"
    assert late.harness_session_id is None


def test_reconcile_rejects_reused_pid_when_original_mux_pane_exited(
    tmp_path: Path, monkeypatch
) -> None:
    """Legacy rows without a start token require their exact mux pane to live."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="late", provider="codex", status="spawning", pid=4242,
             mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_args, **_kwargs: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args, **_kwargs: False)
    monkeypatch.setattr(
        mux_spawn, "_codex_session_id_for_pid",
        lambda _pid: pytest.fail("an exited pane must not donate a rollout"),
    )

    dispatch.reconcile_agents(codex_session_index_path=tmp_path / "missing-index.jsonl")

    row = load_registry()[0]
    assert row.status == "orphaned"
    assert row.harness_session_id is None


def test_reconcile_preserves_legacy_row_when_mux_liveness_is_unknown(
    tmp_path: Path, monkeypatch
) -> None:
    """An unavailable mux is not proof that a reused PID owns the old pane."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="late", provider="codex", status="spawning", pid=4242,
             mux={"session": "missing", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_args, **_kwargs: True)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(
        mux_spawn, "_codex_session_id_for_pid",
        lambda _pid: pytest.fail("unknown pane liveness must not donate a rollout"),
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    assert load_registry()[0].status == "spawning"
    assert any(e["reason"] == "mux-pane-liveness-unavailable" for e in result.errors)


def test_reconcile_preserves_pending_row_when_process_token_is_unreadable(
    tmp_path: Path, monkeypatch
) -> None:
    """A live pane plus unreadable incarnation evidence is unknown, not dead."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="late", provider="codex", status="spawning", pid=4242,
             pid_start_time=123, mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(
        mux_spawn, "_codex_session_id_for_pid",
        lambda _pid: pytest.fail("unknown process identity must not donate a rollout"),
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    assert load_registry()[0].status == "spawning"
    assert any(
        e["reason"] == "codex-process-incarnation-unavailable"
        for e in result.errors
    )


def test_reconcile_recovers_pidless_live_codex_pane(
    tmp_path: Path, monkeypatch
) -> None:
    """A transient spawn-time pane-ls miss converges on the exact pane later."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(name="late", provider="codex", status="spawning", pid=None,
             mux={"session": "main", "pane_id": 7}),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *_args: 4242)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(mux_spawn, "_codex_session_id_for_pid", lambda _pid: session_id)
    monkeypatch.setattr(spawn_gate, "_process_start_time", lambda _pid: 123456)
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_args, **_kwargs: True)

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    row = load_registry()[0]
    assert row.pid == 4242
    assert row.pid_start_time == 123456
    assert row.harness_session_id == session_id
    assert row.status == "live"
    assert result.backfilled[0]["harness_session_id"] == session_id


@pytest.mark.parametrize("status", ["orphaned", "failed", "exited", "permanent_dead"])
def test_reconcile_never_backfills_terminal_codex_rows(
    tmp_path: Path, monkeypatch, status: str
) -> None:
    """AC4-ERR: terminal lifecycle truth wins over a late rollout."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            status=status,
            pid=4242,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _started: True)
    monkeypatch.setattr(
        mux_spawn,
        "_codex_session_id_for_pid",
        lambda _pid: pytest.fail("terminal row must not be probed"),
    )

    dispatch.reconcile_agents(codex_session_index_path=tmp_path / "missing-index.jsonl")

    row = load_registry()[0]
    assert row.status == status
    assert row.harness_session_id is None


@pytest.mark.parametrize("status", ["failed", "exited", "permanent_dead"])
def test_reconcile_never_resurrects_identified_terminal_codex_rows(
    tmp_path: Path, monkeypatch, status: str
) -> None:
    """Terminal lifecycle truth remains absorbing after identity repair."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            harness_session_id=session_id,
            status=status,
            pid=4242,
            mux={"session": "main", "pane_id": 7},
        ),
    )
    index = tmp_path / "session-index.jsonl"
    index.write_text(json.dumps({"id": session_id}) + "\n", encoding="utf-8")

    from fno.agents import dispatch
    from fno.agents.registry import load_registry

    result = dispatch.reconcile_agents(codex_session_index_path=index)

    assert load_registry()[0].status == status
    assert result.recovered == []


def test_reconcile_orphans_a_pending_codex_pane_after_process_exit(
    tmp_path: Path, monkeypatch
) -> None:
    """AC4-ERR: process death keeps the existing orphan lifecycle authoritative."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            status="spawning",
            pid=4242,
            pid_start_time=123,
            mux={"session": "main", "pane_id": 7},
        ),
    )

    from fno.agents import dispatch, mux_spawn, spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _started: False)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *_args: True)
    monkeypatch.setattr(
        mux_spawn,
        "_codex_session_id_for_pid",
        lambda _pid: pytest.fail("an exited process must not be probed"),
    )

    result = dispatch.reconcile_agents(
        codex_session_index_path=tmp_path / "missing-index.jsonl"
    )

    row = load_registry()[0]
    assert row.status == "orphaned"
    assert row.harness_session_id is None
    assert result.orphaned == [
        {"name": "worker-codex", "provider": "codex", "id": None}
    ]


# ---------------------------------------------------------------------------
# attach_agent — AC7-*
# ---------------------------------------------------------------------------


def test_attach_claude_inherits_stdio_and_propagates_exit(
    tmp_path: Path, monkeypatch
) -> None:
    """AC7-HP: claude attach returns claude's exit code."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    calls: list[str] = []

    def fake_attach(short_id: str) -> int:
        calls.append(short_id)
        return 0

    monkeypatch.setattr(claude_mod, "claude_attach", fake_attach)

    result = dispatch.attach_agent("worker-claude")
    assert result.provider == "claude"
    assert result.exit_code == 0
    assert calls == ["7c5dcf5d"]


def test_attach_codex_refused_with_exit_13(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC7-ERR: codex attach exits 13 with explanatory stderr; no subprocess."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    called = False

    def fake_attach(short_id):
        nonlocal called
        called = True
        return 0

    monkeypatch.setattr(claude_mod, "claude_attach", fake_attach)

    result = dispatch.attach_agent("worker-codex")
    assert result.exit_code == 13
    assert called is False
    err = capsys.readouterr().err
    assert "one-shot" in err
    assert "Phase 6" in err


def test_attach_agent_not_found(tmp_path: Path, monkeypatch) -> None:
    """AC7-UI: missing agent name exits 2, no subprocess."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents import dispatch

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.attach_agent("ghost")
    assert exc_info.value.exit_code == 2


def test_attach_claude_propagates_nonzero_exit(
    tmp_path: Path, monkeypatch
) -> None:
    """AC7-EDGE: claude attach exit 4 surfaces as result.exit_code=4."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod, "claude_attach", lambda short_id: 4
    )

    result = dispatch.attach_agent("worker-claude")
    assert result.exit_code == 4


# ---------------------------------------------------------------------------
# Sigma-review follow-ups: tests for fixes landed in the review-fixes commit.
# ---------------------------------------------------------------------------


def test_rm_claude_timeout_preserves_registry(
    tmp_path: Path, monkeypatch
) -> None:
    """Sigma #3: rm timeout must NOT mutate the registry.

    Atomicity invariant (Locked Decision 6): claude shellout FIRST,
    registry mutation AFTER. A timeout in the shellout layer must leave
    the registry untouched and emit the agent_removed event with
    timed_out=true + registry_changed=false.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    def fake_rm(short_id, *, timeout=30.0):
        raise subprocess.TimeoutExpired(
            cmd=["claude", "rm", short_id], timeout=timeout
        )

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 15
    entries = load_registry()
    assert len(entries) == 1, "registry must stay intact on timeout"

    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0].get("timed_out") is True
    assert rm_events[0]["registry_changed"] is False
    assert rm_events[0]["claude_exit"] is None


def test_rm_print_lands_after_registry_write(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Sigma C3: 'removed:' confirmation must NOT print until update_registry succeeds.

    Forces update_registry to raise OSError; the operator must see the
    claude shellout already happened (event emitted) but must NOT see a
    'removed:' confirmation in stdout — that print is now gated on
    registry write success.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (0, ""),
    )

    def boom(*args, **kwargs):
        raise OSError("disk full")

    monkeypatch.setattr(dispatch_mod, "update_registry", boom)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 12
    out = capsys.readouterr().out
    assert "removed: worker-claude" not in out, (
        "stdout must not lie about removal when the registry write fails"
    )


def test_reconcile_skips_claude_when_cli_missing(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Sigma C1: missing claude CLI must NOT mass-orphan every claude agent.

    Mirrors AC3-EDGE for the codex side: when reachability cannot be
    probed, statuses stay untouched and the entries land in `errors`
    with a precise reason.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            status="live",
        ),
        dict(
            name="worker-claude-2",
            provider="claude",
            short_id="abcd1234",
            status="live",
        ),
    )
    # Force claude OFF PATH so is_provider_available returns False.
    monkeypatch.setenv("PATH", "/nonexistent")

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    probe_called = False

    def fake_probe(short_id, *, timeout=10.0):
        nonlocal probe_called
        probe_called = True
        return False

    monkeypatch.setattr(claude_mod, "claude_logs_reachable", fake_probe)

    result = dispatch.reconcile_agents()

    assert probe_called is False, (
        "claude_logs_reachable must NOT be called when claude is not on PATH"
    )
    assert len(result.errors) == 2
    assert all(
        e["reason"] == "claude-cli-not-on-path" for e in result.errors
    )
    # Statuses must stay 'live' — false mass-orphaning would be the C1 bug.
    for entry in load_registry():
        assert entry.status == "live"
    err = capsys.readouterr().err
    assert "claude CLI not on PATH" in err


def test_rm_force_removes_orphan_row_without_short_id(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1: rm --force on a corrupted claude row (no short_id) must
    drop the registry entry instead of refusing.

    The pre-fix code raised DispatchAskError(exit_code=12) before checking
    --force. Help text told the operator to retry with --force, but --force
    was never honored — the row stayed forever.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude"),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    spawn_called = False

    def fake_rm(*args, **kwargs):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    result = dispatch.rm_agent("worker-claude", force=True)

    assert result.registry_changed is True
    assert result.force is True
    assert result.claude_exit is None
    assert load_registry() == []
    # No subprocess fired - we can't shell out without a short_id.
    assert spawn_called is False
    err = capsys.readouterr().err
    assert "registry entry has no short id" in err


def test_rm_without_force_on_orphan_row_still_refuses(
    tmp_path: Path, monkeypatch
) -> None:
    """The non-force path on a corrupted row keeps the legacy refusal so
    operators see the diagnostic and choose to add --force explicitly."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude"),
    )

    from fno.agents import dispatch
    from fno.agents.registry import load_registry

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")  # default force=False

    assert exc_info.value.exit_code == 12
    assert "no short id" in str(exc_info.value)
    # Registry untouched.
    assert len(load_registry()) == 1


def test_rm_uses_locked_short_id_after_concurrent_recreate(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1 round-3: rm must re-resolve the registry entry UNDER the lock.

    Scenario: Process A enters rm_agent, resolves the pre-flock entry
    (short_id=A). Before A acquires the flock, Process B removes and
    recreates the same agent name with short_id=B. When A acquires the
    flock, it MUST shell out `claude rm B` (current truth), NOT
    `claude rm A` (stale pre-flock snapshot).

    Simulated via monkeypatching _resolve_registry_entry to return
    different entries on its two call-sites (pre-flock vs locked).
    """
    use_tmpdir(monkeypatch, tmp_path)
    # Seed with the LATER (post-recreate) short_id so the locked re-resolve
    # picks it up via the real load_registry path.
    _seed_registry(
        dict(name="racy", provider="claude", short_id="bbbbbbbb"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import AgentEntry

    # Patch _resolve_registry_entry to return a STALE entry on the first
    # call (pre-flock fast-fail) and fall through to the real resolver
    # for subsequent calls (the locked re-resolve will read the seeded
    # registry).
    real_resolve = dispatch._resolve_registry_entry
    call_count = {"n": 0}

    def staged_resolve(name: str, **kwargs):
        # kwargs absorbs registry_path forwarding from
        # with_agent_lock_and_entry (Codex P2 on PR #317).
        call_count["n"] += 1
        if call_count["n"] == 1:
            # Stale entry — pretends short_id was aaaaaaaa pre-flock.
            return AgentEntry(
                name=name, harness="claude", cwd="/tmp", log_path="/tmp/x",
                short_id="aaaaaaaa",
            )
        return real_resolve(name, **kwargs)

    monkeypatch.setattr(dispatch, "_resolve_registry_entry", staged_resolve)

    received: list[str] = []

    def fake_rm(short_id, *, timeout=30.0):
        received.append(short_id)
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    dispatch.rm_agent("racy")

    # The shellout must target the locked-resolve short_id, NOT the stale
    # pre-flock one. If the bug regressed, received would equal ["aaaaaaaa"].
    assert received == ["bbbbbbbb"], (
        f"rm_agent shelled out with stale short_id: {received!r}"
    )
    assert call_count["n"] >= 2, "expected at least 2 _resolve calls (pre-flock + locked)"


def test_reconcile_preserves_claude_status_on_probe_error(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1 round-5: transient claude probe failures (timeout / OSError)
    must NOT flip healthy agents to orphaned.

    Pre-fix, claude_logs_reachable returned False on timeout / OSError,
    and reconcile interpreted False as "supervisor lost session" →
    orphaned flip. A single slow probe could mass-orphan the fleet.

    Fix: claude_logs_reachable raises ReachabilityProbeError on
    inconclusive outcomes; reconcile catches and routes to errors
    with reason=claude-probe-failed, preserving status.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            status="live",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError
    from fno.agents.registry import load_registry

    def boom(short_id, *, timeout=10.0):
        raise ReachabilityProbeError(
            provider="claude", reason="timeout after 10s"
        )

    monkeypatch.setattr(claude_mod, "claude_logs_reachable", boom)

    result = dispatch.reconcile_agents()

    # Routed to errors, NOT orphaned/recovered.
    assert len(result.orphaned) == 0
    assert len(result.recovered) == 0
    assert len(result.errors) == 1
    assert result.errors[0]["reason"].startswith("claude-probe-failed")
    # Status preserved.
    assert load_registry()[0].status == "live"


def test_claude_logs_reachable_raises_on_timeout(
    tmp_path: Path, monkeypatch
) -> None:
    """Direct unit-level coverage of the tri-state probe behavior."""
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError

    def slow_run(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd=args[0], timeout=10.0)

    monkeypatch.setattr(claude_mod, "_subprocess_run", slow_run)

    with pytest.raises(ReachabilityProbeError) as exc_info:
        claude_mod.claude_logs_reachable("7c5dcf5d", timeout=10.0)
    assert "timeout" in exc_info.value.reason


def test_claude_logs_reachable_raises_on_oserror(
    tmp_path: Path, monkeypatch
) -> None:
    """OSError (permission, device error) also raises the probe-error,
    not a silent False."""
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError

    def broken_run(*args, **kwargs):
        raise PermissionError("EACCES")

    monkeypatch.setattr(claude_mod, "_subprocess_run", broken_run)

    with pytest.raises(ReachabilityProbeError):
        claude_mod.claude_logs_reachable("7c5dcf5d", timeout=10.0)


def test_reconcile_codex_index_stat_permission_does_not_abort(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1 round-4: a PermissionError from session_index_exists must
    NOT propagate out of reconcile_agents.

    Pre-fix: Path.exists() can raise PermissionError when the parent
    directory is unreadable, which used to abort the whole reconcile
    call. Fix wraps the probe in OSError catch and treats the codex
    side as 'unreadable' — codex agents land in errors, non-codex
    agents still get reconciled.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
        ),
        dict(name="worker-gemini", provider="gemini",
             gemini_session_id="g-1"),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    def boom(*args, **kwargs):
        raise PermissionError("simulated: ~/.codex unreadable")

    monkeypatch.setattr(codex_mod, "session_index_exists", boom)

    # Should NOT raise — must classify and continue.
    result = dispatch.reconcile_agents()

    # Codex side: routed to errors with unreadable reason.
    codex_errors = [e for e in result.errors if e["provider"] == "codex"]
    assert len(codex_errors) == 1
    assert codex_errors[0]["reason"] == "codex-session-index-unreadable"
    # Status untouched.
    codex_entry = next(e for e in load_registry() if e.name == "worker-codex")
    assert codex_entry.status == "live"

    # Legacy Gemini rows are retained for read compatibility but their
    # retired adapter is not probed.
    gemini_errors = [e for e in result.errors if e["provider"] == "gemini"]
    assert len(gemini_errors) == 1
    assert gemini_errors[0]["reason"] == "retired-provider"

    err = capsys.readouterr().err
    assert "codex session index path unreadable" in err


def test_reconcile_codex_index_unreadable_routes_to_errors(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1: codex_session_index unreadable must NOT mass-orphan
    every codex agent. Each entry lands in `errors` with
    reason=codex-session-index-unreadable; statuses stay 'live'.
    """
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id=session_id,
            status="live",
        ),
        dict(
            name="worker-codex-2",
            provider="codex",
            codex_session_id="ffffffff-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.providers.base import ReachabilityProbeError
    from fno.agents.registry import load_registry

    # Create a real file (so session_index_exists returns True) but
    # monkeypatch load_known_session_ids to simulate a read error.
    index = tmp_path / "session_index.jsonl"
    index.write_text("placeholder")
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: index
    )

    def fake_loader(*, session_index_path=None):
        raise ReachabilityProbeError(
            provider="codex", reason="permission denied"
        )

    monkeypatch.setattr(codex_mod, "load_known_session_ids", fake_loader)

    result = dispatch.reconcile_agents()

    # All codex entries land in errors with the precise reason.
    assert len(result.errors) == 2
    assert all(
        e["reason"] == "codex-session-index-unreadable" for e in result.errors
    )
    # Statuses must stay 'live' — false mass-orphaning would be the P1 bug.
    for entry in load_registry():
        assert entry.status == "live"
    err = capsys.readouterr().err
    assert "codex session index unreadable" in err


def test_reconcile_entries_share_key_schema(
    tmp_path: Path, monkeypatch
) -> None:
    """Sigma #3 (type-design): every list entry exposes the same key set.

    Pre-fix, the unknown-provider branch in reconcile omitted the ``id``
    key while other branches included it (sometimes None, sometimes a
    real id). Result: a consumer doing ``entry['id']`` could KeyError
    on the corner case. The fix normalizes every entry to a uniform
    schema; this test pins it.

    Tested via the reachable paths: gemini (skipped) and codex with
    a missing session-id (errors with id=None). Together they cover
    both lists' schemas.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-gemini", provider="gemini",
             gemini_session_id="some-id"),
        dict(name="worker-codex", provider="codex",
             codex_session_id=None),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod

    # Force the codex index path so the codex branch runs to the
    # missing-session-id check (rather than codex-index-missing).
    index = tmp_path / "session_index.jsonl"
    index.write_text("")  # exists but empty
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: index
    )

    result = dispatch.reconcile_agents()

    required = {"name", "provider", "id", "reason"}
    for entry in result.skipped + result.errors:
        keys = set(entry.keys())
        assert required.issubset(keys), (
            f"entry {entry!r} missing one of {required}; has {keys}"
        )
