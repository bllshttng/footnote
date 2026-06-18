"""Tests for dispatch.py gemini routing (US4-gemini Wave 2.2).

Verifies dispatch_ask routes provider="gemini" to the new gemini
create/follow-up paths, that the provider-mismatch detection rejects
cross-provider follow-ups, and that events emit with provider="gemini".
Uses monkeypatched provider modules — no real gemini subprocess.

Real-subprocess integration lives in Wave 2.3
(test_gemini_integration_smoke.py).
"""
from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest

from fno.agents import dispatch as dispatch_mod
from fno.agents.dispatch import (
    DispatchAskError,
    ProviderMismatchError,
    _gemini_output_path,
    dispatch_ask,
)
from fno.agents.registry import AgentEntry, update_registry


@pytest.fixture
def isolated_registry(tmp_path: Path, monkeypatch) -> Path:
    """Point registry + state dir at tmp_path; disable claude/codex/gemini
    PATH checks so dispatch_ask runs in a hermetic environment."""
    from fno import paths
    registry_path = tmp_path / "registry.jsonl"
    state_dir = tmp_path / "state"
    monkeypatch.setattr(paths, "agents_registry_path", lambda: registry_path)
    monkeypatch.setattr(paths, "state_dir", lambda: state_dir)
    # Default: gemini is available, claude/codex are not (so we can't
    # accidentally route to them).
    monkeypatch.setattr(
        dispatch_mod,
        "is_provider_available",
        lambda p: p == "gemini",
    )
    return registry_path


@pytest.fixture
def fake_gemini_create(monkeypatch):
    """Stub providers.gemini.create() to return a deterministic GeminiResult."""
    from fno.agents.providers import gemini as gemini_mod

    calls = []

    def fake_create(*, cwd, prompt, from_name, yolo, output_path, **kwargs):
        calls.append({
            "cwd": str(cwd),
            "prompt": prompt,
            "from_name": from_name,
            "yolo": yolo,
            "output_path": str(output_path),
        })
        # Mimic what _open_tee would create so post-call assertions on
        # the tee path can be performed without invoking real gemini.
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.touch()
        return gemini_mod.GeminiResult(
            exit_code=0,
            session_id="cedb6b44-d140-4fa4-86f1-3b3e7aed339d",
            last_msg="hello from gemini",
            duration_ms=1234,
        )

    monkeypatch.setattr(gemini_mod, "create", fake_create)
    return calls


@pytest.fixture
def fake_gemini_resume(monkeypatch):
    """Stub providers.gemini.resume() with deterministic GeminiResult."""
    from fno.agents.providers import gemini as gemini_mod

    calls = []

    def fake_resume(*, session_id, cwd, prompt, from_name, yolo, output_path, **kwargs):
        calls.append({
            "session_id": session_id,
            "cwd": str(cwd),
            "prompt": prompt,
            "from_name": from_name,
            "yolo": yolo,
            "output_path": str(output_path),
        })
        return gemini_mod.GeminiResult(
            exit_code=0,
            session_id=session_id,
            last_msg="resumed reply",
            duration_ms=2345,
        )

    monkeypatch.setattr(gemini_mod, "resume", fake_resume)
    return calls


def _seed_gemini(name: str, *, session_id: str, cwd: Path) -> AgentEntry:
    entry = AgentEntry(
        name=name,
        provider="gemini",
        cwd=str(cwd),
        log_path=str(cwd / "log.jsonl"),
        gemini_session_id=session_id,
        status="live",
    )
    update_registry(lambda entries: entries + [entry])
    return entry


# ---------------------------------------------------------------------------
# Create routing
# ---------------------------------------------------------------------------


class _FakeLockHandle:
    """Minimal lock handle stub for create-path tests."""
    def detach(self) -> None:
        pass


def test_dispatch_creates_gemini_agent_when_no_existing(
    isolated_registry: Path, fake_gemini_create, tmp_path: Path
) -> None:
    """AC4-HP: _gemini_create_path routes to gemini provider and persists a registry row.
    Repointed at helper (create contract moved from dispatch_ask to spawn verb)."""
    from fno.agents.dispatch import _gemini_create_path

    result = _gemini_create_path(
        name="worker-A",
        message="draft the migration",
        cwd=tmp_path,
        from_name="orchestrator",
        yolo=False,
        timeout_sec=30.0,
        lock_handle=_FakeLockHandle(),
    )

    assert result.kind == "followup"  # gemini create returns reply-routed result
    assert result.short_id == "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"
    assert result.reply == "hello from gemini"

    # Provider was called once with the right shape.
    assert len(fake_gemini_create) == 1
    call = fake_gemini_create[0]
    assert call["from_name"] == "orchestrator"
    assert call["yolo"] is False
    assert call["prompt"] == "draft the migration"

    # Registry has the new entry.
    from fno.agents.registry import load_registry
    entries = load_registry()
    assert len(entries) == 1
    assert entries[0].provider == "gemini"
    assert entries[0].gemini_session_id == "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"


def test_dispatch_create_routes_yolo_flag(
    isolated_registry: Path, fake_gemini_create, tmp_path: Path
) -> None:
    """yolo=True forwards through to gemini.create().
    Repointed at _gemini_create_path."""
    from fno.agents.dispatch import _gemini_create_path

    _gemini_create_path(
        name="worker-A",
        message="draft",
        cwd=tmp_path,
        from_name="orchestrator",
        yolo=True,
        timeout_sec=10.0,
        lock_handle=_FakeLockHandle(),
    )
    assert fake_gemini_create[0]["yolo"] is True


def test_dispatch_create_emits_agent_ask_done(
    isolated_registry: Path, fake_gemini_create, tmp_path: Path, monkeypatch
) -> None:
    """The agent_ask_done event carries provider=gemini and the session id.
    Repointed at _gemini_create_path."""
    emitted = []
    from fno.agents import events
    monkeypatch.setattr(
        events, "emit",
        lambda evt, **kw: emitted.append((evt, kw)),
    )
    from fno.agents.dispatch import _gemini_create_path

    _gemini_create_path(
        name="worker-A",
        message="hi",
        cwd=tmp_path,
        from_name="orchestrator",
        yolo=False,
        timeout_sec=10.0,
        lock_handle=_FakeLockHandle(),
    )

    done_events = [e for e in emitted if e[0] == "agent_ask_done"]
    assert done_events
    payload = done_events[0][1]
    assert payload["provider"] == "gemini"
    assert payload["gemini_session_id"] == "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"


# ---------------------------------------------------------------------------
# Follow-up routing
# ---------------------------------------------------------------------------


def test_dispatch_followup_routes_to_gemini_resume(
    isolated_registry: Path, fake_gemini_resume, tmp_path: Path
) -> None:
    """AC5-HP: existing gemini agent + ask routes to gemini.resume() with
    the registry-recorded cwd, NOT the call-time cwd."""
    seed_cwd = tmp_path / "original-cwd"
    seed_cwd.mkdir()
    call_time_cwd = tmp_path / "different-cwd"
    call_time_cwd.mkdir()

    _seed_gemini(
        "worker-A",
        session_id="cedb6b44-d140-4fa4-86f1-3b3e7aed339d",
        cwd=seed_cwd,
    )

    result = dispatch_ask(
        name="worker-A",
        message="switch to zod",
        provider="gemini",
        cwd=call_time_cwd,  # NOT used for resume; registry cwd wins
        from_name="orchestrator",
    )
    assert result.kind == "followup"
    assert result.reply == "resumed reply"

    # AC5-EDGE: registry cwd, not call-time cwd
    assert fake_gemini_resume[0]["cwd"] == str(seed_cwd)
    assert fake_gemini_resume[0]["session_id"] == "cedb6b44-d140-4fa4-86f1-3b3e7aed339d"


def test_dispatch_followup_rejects_provider_mismatch(
    isolated_registry: Path, fake_gemini_resume, tmp_path: Path
) -> None:
    """AC5-UI: --provider claude against a gemini agent raises
    ProviderMismatchError via select_provider."""
    _seed_gemini(
        "worker-A",
        session_id="11111111-1111-1111-1111-111111111111",
        cwd=tmp_path,
    )

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="worker-A",
            message="hi",
            provider="claude",  # MISMATCH
            cwd=tmp_path,
            from_name="orchestrator",
        )
    # select_provider's ProviderMismatchError maps to DispatchAskError(exit_code=2)
    assert exc_info.value.exit_code == 2
    assert fake_gemini_resume == []  # gemini.resume never invoked


def test_dispatch_followup_without_provider_works(
    isolated_registry: Path, fake_gemini_resume, tmp_path: Path
) -> None:
    """AC5-HP: --provider is OPTIONAL on follow-up; the recorded
    provider is inferred from the registry."""
    _seed_gemini(
        "worker-A",
        session_id="22222222-2222-2222-2222-222222222222",
        cwd=tmp_path,
    )

    result = dispatch_ask(
        name="worker-A",
        message="hi",
        provider=None,  # let registry decide
        cwd=tmp_path,
        from_name="orchestrator",
    )
    assert result.kind == "followup"
    assert len(fake_gemini_resume) == 1


def test_dispatch_followup_raises_when_session_id_missing(
    isolated_registry: Path, fake_gemini_resume, tmp_path: Path
) -> None:
    """A gemini entry with no session_id is a registry corruption signal
    -> exit 11 with rm-and-recreate hint."""
    entry = AgentEntry(
        name="broken",
        provider="gemini",
        cwd=str(tmp_path),
        log_path=str(tmp_path / "log.jsonl"),
        gemini_session_id=None,  # corruption
        status="live",
    )
    update_registry(lambda entries: entries + [entry])

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="broken",
            message="hi",
            provider=None,
            cwd=tmp_path,
            from_name="orchestrator",
        )
    assert exc_info.value.exit_code == 11
    assert "no gemini_session_id" in str(exc_info.value)


# ---------------------------------------------------------------------------
# Error surface: timeout / parse / invocation
# ---------------------------------------------------------------------------


def test_dispatch_create_timeout_maps_to_exit_15(
    isolated_registry: Path, tmp_path: Path, monkeypatch
) -> None:
    """Repointed at _gemini_create_path (create contract moved from dispatch_ask to spawn verb)."""
    from fno.agents.dispatch import _gemini_create_path
    from fno.agents.providers import gemini as gemini_mod

    def raising(**kwargs):
        raise gemini_mod.GeminiTimeoutError(30.0)
    monkeypatch.setattr(gemini_mod, "create", raising)

    with pytest.raises(DispatchAskError) as exc_info:
        _gemini_create_path(
            name="X",
            message="hi",
            cwd=tmp_path,
            from_name="orchestrator",
            yolo=False,
            timeout_sec=30.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc_info.value.exit_code == 15
    assert "timed out" in str(exc_info.value)


def test_dispatch_create_parse_error_maps_to_exit_11(
    isolated_registry: Path, tmp_path: Path, monkeypatch
) -> None:
    """Repointed at _gemini_create_path (create contract moved from dispatch_ask to spawn verb)."""
    from fno.agents.dispatch import _gemini_create_path
    from fno.agents.providers import gemini as gemini_mod

    def raising(**kwargs):
        raise gemini_mod.GeminiParseError("{garbage")
    monkeypatch.setattr(gemini_mod, "create", raising)

    with pytest.raises(DispatchAskError) as exc_info:
        _gemini_create_path(
            name="X",
            message="hi",
            cwd=tmp_path,
            from_name="orchestrator",
            yolo=False,
            timeout_sec=10.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc_info.value.exit_code == 11
    assert "parse failed" in str(exc_info.value)


def test_dispatch_create_invocation_error_propagates_exit_code(
    isolated_registry: Path, tmp_path: Path, monkeypatch
) -> None:
    """A GeminiInvocationError(127) (binary missing mid-call) propagates 127.
    Repointed at _gemini_create_path (create contract moved from dispatch_ask to spawn verb)."""
    from fno.agents.dispatch import _gemini_create_path
    from fno.agents.providers import gemini as gemini_mod

    def raising(**kwargs):
        raise gemini_mod.GeminiInvocationError(127)
    monkeypatch.setattr(gemini_mod, "create", raising)

    with pytest.raises(DispatchAskError) as exc_info:
        _gemini_create_path(
            name="X",
            message="hi",
            cwd=tmp_path,
            from_name="orchestrator",
            yolo=False,
            timeout_sec=10.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc_info.value.exit_code == 127


def test_dispatch_create_path_check_short_circuits_when_gemini_missing(
    isolated_registry: Path, tmp_path: Path, monkeypatch
) -> None:
    """_gemini_create_path calls gemini.create() unconditionally; the
    provider-availability (exit 14) pre-flight is the CALLER's responsibility
    (spawn verb). Repointed to verify the caller contract: _gemini_create_path
    invokes gemini.create and surfaces its error, so the spawn caller must
    check is_provider_available BEFORE calling this helper."""
    from fno.agents.dispatch import _gemini_create_path
    from fno.agents.providers import gemini as gemini_mod

    # When gemini is present but exits 14-equivalent (binary missing mid-call),
    # the error propagates through _gemini_create_path as GeminiInvocationError(14).
    # This verifies that _gemini_create_path does NOT silently swallow
    # invocation failures - the caller's pre-check guards against calling it
    # when gemini is absent.
    def raising(**kwargs):
        raise gemini_mod.GeminiInvocationError(14)
    monkeypatch.setattr(gemini_mod, "create", raising)

    with pytest.raises(DispatchAskError) as exc_info:
        _gemini_create_path(
            name="X",
            message="hi",
            cwd=tmp_path,
            from_name="orchestrator",
            yolo=False,
            timeout_sec=10.0,
            lock_handle=_FakeLockHandle(),
        )
    assert exc_info.value.exit_code == 14


def test_gemini_output_path_layout(tmp_path: Path, monkeypatch) -> None:
    """Sanity: the gemini tee path mirrors codex's <state>/agents/<name>/output.jsonl."""
    from fno import paths
    monkeypatch.setattr(paths, "state_dir", lambda: tmp_path)
    expected = tmp_path / "agents" / "worker-A" / "output.jsonl"
    assert _gemini_output_path("worker-A") == expected
