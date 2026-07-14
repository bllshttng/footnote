"""Dispatch-layer tests for codex routing (Wave 2.1).

Verifies ``dispatch_ask`` routes to ``providers.codex`` correctly for both
create and resume paths. Provider invocation is monkeypatched so the
dispatch logic is exercised in isolation; real subprocess testing lives
in ``test_codex_integration_smoke.py`` and ``test_codex_signal_handling.py``.

Plan ACs covered:
- AC1-HP create succeeds, registers codex_session_id, exit 0
- AC1-ERR codex exits non-zero, registry untouched
- AC1-EDGE 0-line JSONL -> exit 11, registry untouched
- AC1-UI session_id reachable via registry after create
- AC2-HP follow-up resumes session, bumps last_message_at
- AC2-ERR follow-up against missing/invalid session propagates error
- AC2-UI provider-mismatch rejected at dispatch
- AC2-EDGE follow-up cwd defaults to registered cwd
- AC2-FR follow-up timeout SIGTERMs codex, exit 15
- AC3-HP --yolo passes --dangerously-bypass-...
- AC3-ERR --yolo no-op for claude with stderr note
- AC3-UI default sandbox is workspace-write
- AC3-EDGE --yolo recorded in events.jsonl
- AC4-HP from-name appears as bracket prefix
- AC4-ERR invalid from_name rejected pre-subprocess
- AC4-UI default from_name 'fno'
"""
from __future__ import annotations

import json
import shutil
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from fno.paths_testing import use_tmpdir
from fno.agents import events as events_mod
from fno.agents.providers import codex as codex_mod
from fno.agents.providers.codex import (
    CodexInvocationError,
    CodexResult,
    CodexTimeoutError,
    NoSessionIdError,
)
from fno.agents.registry import (
    AgentEntry,
    load_registry,
    write_registry,
)


@pytest.fixture
def workdir(tmp_path, monkeypatch):
    """Isolated fno home with codex marked available on PATH."""
    use_tmpdir(monkeypatch, tmp_path)
    # Stub PATH so `which("codex")` returns truthy.
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text("#!/bin/sh\necho fake\n", encoding="utf-8")
    (bin_dir / "codex").chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


@pytest.fixture
def fake_codex_create(monkeypatch):
    """Replace codex_mod.create with a MagicMock returning a happy CodexResult.

    Tests can mutate the return_value or side_effect to script failures.
    """
    mock = MagicMock(return_value=CodexResult(
        exit_code=0,
        session_id="codex-sid-abc",
        last_msg="hello",
        duration_ms=42,
    ))
    monkeypatch.setattr(codex_mod, "create", mock)
    return mock


@pytest.fixture
def fake_codex_resume(monkeypatch):
    mock = MagicMock(return_value=CodexResult(
        exit_code=0,
        session_id="codex-sid-abc",
        last_msg="follow-up reply",
        duration_ms=22,
    ))
    monkeypatch.setattr(codex_mod, "resume", mock)
    return mock


def _read_events() -> list[dict]:
    """Return parsed events.jsonl entries for assertions."""
    from fno import paths
    log = paths.state_dir() / "events.jsonl"
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text().splitlines() if line]


# ---------------------------------------------------------------------------
# AC1 — create
# ---------------------------------------------------------------------------


class _FakeLockHandle:
    """Minimal lock handle stub for _codex_create_path / _claude_create_path tests."""
    def detach(self) -> None:
        pass


def test_create_codex_routes_to_provider_and_registers(workdir, fake_codex_create):
    """Repointed at _codex_create_path (create contract moved from dispatch_ask to spawn verb)."""
    from fno.agents.dispatch import _codex_create_path

    result = _codex_create_path(
        name="worker-X",
        message="echo hello",
        cwd=workdir,
        from_name="fno",
        yolo=False,
        timeout_sec=10.0,
        lock_handle=_FakeLockHandle(),
    )

    assert result.kind == "followup"  # codex reply printed verbatim like a followup
    assert result.short_id == "codex-sid-abc"
    assert result.reply == "hello"

    fake_codex_create.assert_called_once()
    call = fake_codex_create.call_args
    assert call.kwargs["cwd"] == workdir
    assert call.kwargs["yolo"] is False
    assert call.kwargs["from_name"] == "fno"
    assert call.kwargs["prompt"] == "echo hello"

    entries = load_registry()
    assert len(entries) == 1
    e = entries[0]
    assert e.name == "worker-X"
    assert e.provider == "codex"
    assert e.codex_session_id == "codex-sid-abc"
    assert e.cwd == str(workdir)
    assert e.status == "live"


def test_create_codex_no_session_id_maps_to_exit_11(workdir, fake_codex_create):
    """Repointed at _codex_create_path."""
    fake_codex_create.side_effect = NoSessionIdError({"turn.started", "turn.completed"})

    from fno.agents.dispatch import DispatchAskError, _codex_create_path
    with pytest.raises(DispatchAskError) as exc:
        _codex_create_path(
            name="worker-X",
            message="msg",
            cwd=workdir,
            from_name="fno",
            yolo=False,
            timeout_sec=5.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc.value.exit_code == 11
    # Registry MUST NOT have a row for a failed create.
    assert load_registry() == []


def test_create_codex_invocation_error_maps_to_exit_1(workdir, fake_codex_create):
    """Repointed at _codex_create_path."""
    fake_codex_create.side_effect = CodexInvocationError(1)
    from fno.agents.dispatch import DispatchAskError, _codex_create_path
    with pytest.raises(DispatchAskError) as exc:
        _codex_create_path(
            name="worker-X",
            message="msg",
            cwd=workdir,
            from_name="fno",
            yolo=False,
            timeout_sec=5.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc.value.exit_code == 1
    assert load_registry() == []


def test_create_codex_propagates_provider_specific_exit_code(workdir, fake_codex_create):
    """Gemini PR #305 round 3: dispatch must propagate CodexInvocationError.exit_code.
    Repointed at _codex_create_path."""
    fake_codex_create.side_effect = CodexInvocationError(12)
    from fno.agents.dispatch import DispatchAskError, _codex_create_path
    with pytest.raises(DispatchAskError) as exc:
        _codex_create_path(
            name="worker-X",
            message="msg",
            cwd=workdir,
            from_name="fno",
            yolo=False,
            timeout_sec=5.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc.value.exit_code == 12

    fake_codex_create.side_effect = CodexInvocationError(127)
    with pytest.raises(DispatchAskError) as exc:
        _codex_create_path(
            name="worker-Y",
            message="msg",
            cwd=workdir,
            from_name="fno",
            yolo=False,
            timeout_sec=5.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc.value.exit_code == 127


def test_followup_codex_propagates_provider_specific_exit_code(workdir, fake_codex_resume):
    """Same propagation on the follow-up path."""
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd=str(workdir),
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id="sid",
        )
    ])
    fake_codex_resume.side_effect = CodexInvocationError(12)
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider=None,
            cwd=workdir,
            timeout=5,
        )
    assert exc.value.exit_code == 12


def test_create_codex_timeout_maps_to_exit_15(workdir, fake_codex_create):
    """Repointed at _codex_create_path."""
    fake_codex_create.side_effect = CodexTimeoutError(2.0)
    from fno.agents.dispatch import DispatchAskError, _codex_create_path
    with pytest.raises(DispatchAskError) as exc:
        _codex_create_path(
            name="worker-X",
            message="msg",
            cwd=workdir,
            from_name="fno",
            yolo=False,
            timeout_sec=2.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc.value.exit_code == 15
    assert load_registry() == []


def test_create_codex_emits_yolo_flag_in_events(workdir, fake_codex_create):
    """Repointed at _codex_create_path. agent_ask_started is emitted by the caller
    (dispatch_ask / spawn verb) before routing; _codex_create_path emits agent_ask_done."""
    from fno.agents.dispatch import _codex_create_path
    _codex_create_path(
        name="worker-bootstrap",
        message="msg",
        cwd=workdir,
        from_name="fno",
        yolo=True,
        timeout_sec=5.0,
        lock_handle=_FakeLockHandle(),
    )
    events = _read_events()
    done = [e for e in events if e.get("kind") == "agent_ask_done"]
    assert done and done[-1].get("yolo") is True


def test_create_codex_passes_yolo_to_provider(workdir, fake_codex_create):
    """Repointed at _codex_create_path."""
    from fno.agents.dispatch import _codex_create_path
    _codex_create_path(
        name="worker-bootstrap",
        message="msg",
        cwd=workdir,
        from_name="fno",
        yolo=True,
        timeout_sec=5.0,
        lock_handle=_FakeLockHandle(),
    )
    fake_codex_create.assert_called_once()
    assert fake_codex_create.call_args.kwargs["yolo"] is True


# ---------------------------------------------------------------------------
# AC2 — follow-up (resume)
# ---------------------------------------------------------------------------


def test_followup_codex_routes_to_resume_and_bumps_last_message_at(
    workdir, fake_codex_resume
):
    # Seed registry with a codex agent.
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd="/Users/foo/proj",
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id="real-uuid",
            created_at="2026-05-21T00:00:00Z",
            status="live",
            last_message_at=None,
        )
    ])

    from fno.agents.dispatch import dispatch_ask
    result = dispatch_ask(
        name="worker-X",
        message="follow up",
        provider=None,
        cwd=workdir,
        timeout=10,
    )
    assert result.kind == "followup"
    assert result.reply == "follow-up reply"

    fake_codex_resume.assert_called_once()
    call = fake_codex_resume.call_args
    # AC2-EDGE: cwd comes from registry, not workdir.
    assert call.kwargs["cwd"] == Path("/Users/foo/proj")
    assert call.kwargs["session_id"] == "real-uuid"

    # last_message_at bumped.
    entries = load_registry()
    assert len(entries) == 1
    assert entries[0].last_message_at is not None
    # codex_session_id preserved (never re-minted).
    assert entries[0].codex_session_id == "real-uuid"


def test_followup_codex_provider_mismatch_rejected(workdir, fake_codex_resume):
    """AC2-UI: ask with --provider claude against codex registry row -> exit 2."""
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd=str(workdir),
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id="sid",
        )
    ])
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider="claude",
            cwd=workdir,
            timeout=5,
        )
    assert exc.value.exit_code == 2
    fake_codex_resume.assert_not_called()


def test_followup_codex_empty_log_path_rejected_at_dispatch(workdir, fake_codex_resume):
    """Gemini PR #305 finding: registry log_path is contract-guarded; an
    empty string is registry corruption, not a recoverable case."""
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd=str(workdir),
            log_path="",  # corrupted
            codex_session_id="sid",
        )
    ])
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider=None,
            cwd=workdir,
            timeout=5,
        )
    assert exc.value.exit_code == 11
    assert "log_path" in str(exc.value)
    fake_codex_resume.assert_not_called()


def test_followup_codex_empty_cwd_rejected_at_dispatch(workdir, fake_codex_resume):
    """Gemini PR #305 finding: registry cwd is contract-guarded; codex
    sessions are cwd-pinned and a follow-up cannot proceed without it."""
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd="",  # corrupted
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id="sid",
        )
    ])
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider=None,
            cwd=workdir,
            timeout=5,
        )
    assert exc.value.exit_code == 11
    assert "cwd" in str(exc.value)
    fake_codex_resume.assert_not_called()


def test_followup_codex_no_session_id_in_registry_rejected(workdir, fake_codex_resume):
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd=str(workdir),
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id=None,  # corrupted state
        )
    ])
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider=None,
            cwd=workdir,
            timeout=5,
        )
    assert exc.value.exit_code == 11
    fake_codex_resume.assert_not_called()


def test_followup_codex_timeout_maps_to_exit_15(workdir, fake_codex_resume):
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd=str(workdir),
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id="sid",
        )
    ])
    fake_codex_resume.side_effect = CodexTimeoutError(2.0)
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider=None,
            cwd=workdir,
            timeout=2,
        )
    assert exc.value.exit_code == 15
    # last_message_at NOT advanced on failure.
    assert load_registry()[0].last_message_at is None


def test_followup_codex_invocation_error_maps_to_exit_1(workdir, fake_codex_resume):
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd=str(workdir),
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id="invalid",
        )
    ])
    fake_codex_resume.side_effect = CodexInvocationError(1)
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider=None,
            cwd=workdir,
            timeout=5,
        )
    assert exc.value.exit_code == 1
    # last_message_at NOT advanced on failure.
    assert load_registry()[0].last_message_at is None


def test_followup_codex_emits_yolo_flag_in_events(workdir, fake_codex_resume):
    write_registry([
        AgentEntry(
            name="worker-X",
            provider="codex",
            cwd=str(workdir),
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            codex_session_id="sid",
        )
    ])
    from fno.agents.dispatch import dispatch_ask
    dispatch_ask(
        name="worker-X",
        message="msg",
        provider="codex",
        cwd=workdir,
        timeout=5,
        yolo=True,
    )
    started = [e for e in _read_events() if e.get("kind") == "agent_followup_started"]
    done = [e for e in _read_events() if e.get("kind") == "agent_followup_done"]
    assert started and started[-1].get("yolo") is True
    assert done and done[-1].get("yolo") is True


# ---------------------------------------------------------------------------
# AC3 — --yolo semantics
# ---------------------------------------------------------------------------


def test_yolo_on_claude_create_maps_to_bypass_permissions(workdir, capsys, monkeypatch):
    """x-dfa4: --yolo for claude create now maps to bypassPermissions (was a
    no-op note). bg_create receives permission_mode=bypassPermissions and the
    misleading 'no effect' note is gone."""
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ProviderResult
    seen: dict = {}

    def fake_bg_create(**kw):
        seen.update(kw)
        return ProviderResult(
            exit_code=0,
            stdout="backgrounded · 7c5dcf5d · worker-Y\n",
            stderr="",
            duration_ms=10,
            session_id_out="7c5dcf5d",
        )

    monkeypatch.setattr(claude_mod, "bg_create", fake_bg_create)

    from fno.agents.dispatch import _claude_create_path
    _claude_create_path(
        name="worker-Y",
        message="msg",
        chosen="claude",
        cwd=workdir,
        timeout=5,
        yolo=True,
        lock_handle=_FakeLockHandle(),
    )
    err = capsys.readouterr().err
    assert "--yolo has no effect" not in err
    assert seen.get("permission_mode") == "bypassPermissions"


def test_yolo_on_claude_followup_emits_stderr_note(workdir, capsys, monkeypatch):
    """AC3-ERR variant: claude follow-up with --yolo also emits the note."""
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers._claude_session_registry import SessionLocator

    write_registry([
        AgentEntry(
            name="worker-Y",
            provider="claude",
            cwd=str(workdir),
            log_path=str(workdir / "agents" / "worker-X" / "output.jsonl"),
            short_id="7c5dcf5d",
            status="live",
        )
    ])
    monkeypatch.setattr(
        claude_mod,
        "ask_followup",
        lambda **kw: "reply",
    )

    from fno.agents.dispatch import dispatch_ask
    dispatch_ask(
        name="worker-Y",
        message="msg",
        provider="claude",
        cwd=workdir,
        timeout=5,
        yolo=True,
    )
    err = capsys.readouterr().err
    assert "--yolo has no effect for provider 'claude'" in err


# ---------------------------------------------------------------------------
# AC4 — from-name
# ---------------------------------------------------------------------------


def test_from_name_prepended_to_codex_prompt(workdir, fake_codex_create):
    """Repointed at _codex_create_path."""
    from fno.agents.dispatch import _codex_create_path
    _codex_create_path(
        name="worker-X",
        message="do the thing",
        cwd=workdir,
        from_name="orchestrator-main",
        yolo=False,
        timeout_sec=5.0,
        lock_handle=_FakeLockHandle(),
    )
    call = fake_codex_create.call_args
    assert call.kwargs["from_name"] == "orchestrator-main"
    # The actual bracket prefix is applied by codex.create() (verified in
    # test_providers_codex_create), but we assert dispatch passes the
    # name through unchanged.


def test_invalid_from_name_rejected_before_subprocess(workdir, fake_codex_create):
    """AC4-ERR: from_name validator rejects XML-unsafe input on the codex path.

    Reuses the US2 validator (no fresh regex per AC1.3 invariant); the rule
    set is: non-empty AND <=128 chars AND no XML-unsafe characters (", <, >, &).
    """
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    for bad in ('', '"bad"', '<inject>', 'amp&persand', 'right>angle'):
        with pytest.raises(DispatchAskError) as exc:
            dispatch_ask(
                name="worker-X",
                message="msg",
                provider="codex",
                cwd=workdir,
                timeout=5,
                from_name=bad,
            )
        assert exc.value.exit_code == 2, f"input {bad!r}"
    # Provider MUST NOT have been called for any of the rejected inputs.
    fake_codex_create.assert_not_called()


def test_yolo_does_not_bypass_from_name_validator(workdir, fake_codex_create):
    """AC3-FR: --yolo MUST NOT bypass the from-name validator."""
    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask(
            name="worker-X",
            message="msg",
            provider="codex",
            cwd=workdir,
            yolo=True,
            from_name='evil"',
            timeout=5,
        )
    assert exc.value.exit_code == 2
    fake_codex_create.assert_not_called()


def test_default_from_name_is_abilities(workdir, fake_codex_create):
    """Repointed at _codex_create_path."""
    from fno.agents.dispatch import _codex_create_path
    _codex_create_path(
        name="worker-X",
        message="msg",
        cwd=workdir,
        from_name="fno",  # default value passed explicitly
        yolo=False,
        timeout_sec=5.0,
        lock_handle=_FakeLockHandle(),
    )
    assert fake_codex_create.call_args.kwargs["from_name"] == "fno"
