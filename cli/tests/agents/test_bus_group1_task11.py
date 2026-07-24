"""Task 1.1: de-overload ask - unknown names error with spawn-it-first (exit 16).

Acceptance criteria (operator-locked):
  AC1-ERR: unregistered name -> stderr "unknown agent" + "spawn it first", exit 16,
           registry unchanged, no provider subprocess invoked.
  AC1-ERR variant: unknown name + --harness codex -> still unknown-agent error, exit 16.
  AC1-HP regression: existing entries still follow up exactly as before.
  AC2-ERR: provider mismatch on existing name -> exit 2 (unchanged).
  AC3-VERIFY: agent_ask_failed with stage="unknown-name" lands in events.jsonl.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_runner() -> CliRunner:
    return CliRunner()


def _write_existing_entry(name: str, provider: str, short_id: str) -> None:
    """Write a minimal registry entry for an existing agent."""
    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name=name,
            harness=provider,
            cwd="/tmp",
            log_path="/tmp/a.log",
            short_id=short_id if provider == "claude" else "",
            harness_session_id=short_id if provider == "codex" else None,
        )
    ])


def _read_events(tmp_path: Path) -> list[dict]:
    from fno import paths
    events_log = paths.state_dir() / "events.jsonl"
    if not events_log.exists():
        return []
    return [
        json.loads(line)
        for line in events_log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


# ---------------------------------------------------------------------------
# AC1-ERR: unknown name -> exit 16, stderr contains usage hint, no registry mutation
# ---------------------------------------------------------------------------


def test_unknown_agent_name_exits_16(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR: unregistered name 'blue' -> exit 16 with unknown-agent message."""
    use_tmpdir(monkeypatch, tmp_path)

    # Monkeypatch provider modules to FAIL the test if invoked (they must NOT be called)
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers import codex as codex_mod

    def _fail_create(*args, **kwargs):
        raise AssertionError("Provider create must NOT be invoked for unknown agent names")

    monkeypatch.setattr(claude_mod, "bg_create", _fail_create)
    monkeypatch.setattr(codex_mod, "create", _fail_create)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "work"
    cwd.mkdir()

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="blue",
            message="hello",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )

    err = exc_info.value
    assert err.exit_code == 16, f"Expected exit 16, got {err.exit_code}"
    msg = str(err)
    assert "unknown agent" in msg
    assert "spawn it first" in msg


def test_unknown_agent_name_registry_unchanged(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR: registry file must remain empty; no new entry created."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.registry import load_registry

    cwd = tmp_path / "work"
    cwd.mkdir()

    with pytest.raises(DispatchAskError):
        dispatch_ask(
            name="blue",
            message="hello",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )

    entries = load_registry()
    assert entries == [], f"Registry must be empty, got {entries}"


def test_unknown_agent_name_no_provider_subprocess(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR: no provider subprocess invoked for unknown agent name."""
    use_tmpdir(monkeypatch, tmp_path)

    # Track invocations - any call means the test fails
    invocations: list[str] = []

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers import codex as codex_mod

    def _record_claude(*args, **kwargs):
        invocations.append("claude.bg_create")
        raise AssertionError("Should not be called")

    def _record_codex(*args, **kwargs):
        invocations.append("codex.create")
        raise AssertionError("Should not be called")

    monkeypatch.setattr(claude_mod, "bg_create", _record_claude)
    monkeypatch.setattr(codex_mod, "create", _record_codex)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "work"
    cwd.mkdir()

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="blue",
            message="hello",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )

    assert exc_info.value.exit_code == 16
    assert invocations == [], f"Provider subprocesses were invoked: {invocations}"


# ---------------------------------------------------------------------------
# AC1-ERR variant: unknown name WITH --harness codex -> still exit 16
# ---------------------------------------------------------------------------


def test_unknown_agent_name_with_provider_still_exits_16(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR variant: unknown name + --harness codex -> still exit 16 (not create)."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.providers import codex as codex_mod

    def _fail_create(*args, **kwargs):
        raise AssertionError("codex.create must NOT be called for unknown agent")

    monkeypatch.setattr(codex_mod, "create", _fail_create)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "work"
    cwd.mkdir()

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="blue",
            message="hello",
            provider="codex",
            cwd=cwd,
            timeout=10,
        )

    err = exc_info.value
    assert err.exit_code == 16
    msg = str(err)
    assert "unknown agent" in msg
    assert "spawn it first" in msg


# ---------------------------------------------------------------------------
# AC1-ERR via CLI: Typer cmd_ask propagates exit 16
# ---------------------------------------------------------------------------


def test_cmd_ask_unknown_agent_stderr_and_exit_16(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR via CLI: unknown agent -> stderr hint + exit 16."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["ask", "blue", "hello", "--harness", "claude"],
    )

    assert result.exit_code == 16, f"Expected 16, got {result.exit_code}. output={result.output}"
    combined = (result.stderr or "") + (result.stdout or "")
    assert "unknown agent" in combined
    assert "spawn it first" in combined


# ---------------------------------------------------------------------------
# AC3-VERIFY: event agent_ask_failed with stage="unknown-name" emitted
# ---------------------------------------------------------------------------


def test_unknown_agent_emits_ask_failed_event(tmp_path: Path, monkeypatch) -> None:
    """AC3-VERIFY: agent_ask_failed with stage='unknown-name' and name='blue' in events.jsonl."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "work"
    cwd.mkdir()

    with pytest.raises(DispatchAskError):
        dispatch_ask(
            name="blue",
            message="hello",
            provider="claude",
            cwd=cwd,
            timeout=10,
        )

    events = _read_events(tmp_path)
    failed_events = [e for e in events if e.get("kind") == "agent_ask_failed"]
    assert failed_events, "Expected at least one agent_ask_failed event"

    unknown_name_events = [
        e for e in failed_events
        if e.get("stage") == "unknown-name"
    ]
    assert unknown_name_events, (
        f"Expected agent_ask_failed with stage='unknown-name', got: {failed_events}"
    )
    assert unknown_name_events[0].get("name") == "blue"


# ---------------------------------------------------------------------------
# AC1-HP regression: existing claude entry follows up as before
# ---------------------------------------------------------------------------


def test_existing_claude_entry_still_follows_up(tmp_path: Path, monkeypatch) -> None:
    """AC1-HP regression: existing claude entry routes to follow-up path (not error)."""
    use_tmpdir(monkeypatch, tmp_path)
    # Set HOME so claude locate_session searches a known-empty dir
    home_dir = tmp_path / "home"
    home_dir.mkdir()
    monkeypatch.setenv("HOME", str(home_dir))

    _write_existing_entry("existing-worker", "claude", "abc12345")

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "work"
    cwd.mkdir()

    # Follow-up will fail at orphan stage (no real claude session) - exit 13.
    # The point is it must NOT exit 16.
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="existing-worker",
            message="new message",
            provider=None,
            cwd=cwd,
            timeout=10,
        )

    # Must be orphan (13) - not unknown-agent (16)
    assert exc_info.value.exit_code == 13, (
        f"Expected exit 13 (orphan on followup), got {exc_info.value.exit_code}: {exc_info.value}"
    )
    assert "unknown agent" not in str(exc_info.value)


# ---------------------------------------------------------------------------
# AC2-ERR: provider mismatch on existing name -> still exit 2
# ---------------------------------------------------------------------------


def test_provider_mismatch_on_existing_still_exits_2(tmp_path: Path, monkeypatch) -> None:
    """AC2-ERR: provider mismatch on known agent remains exit 2 (unchanged)."""
    use_tmpdir(monkeypatch, tmp_path)

    _write_existing_entry("worker", "claude", "abc12345")

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    cwd = tmp_path / "work"
    cwd.mkdir()

    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_ask(
            name="worker",
            message="hello",
            provider="codex",  # wrong provider
            cwd=cwd,
            timeout=10,
        )

    assert exc_info.value.exit_code == 2
    msg = str(exc_info.value)
    assert "provider" in msg.lower()


# ---------------------------------------------------------------------------
# Module constant: UNKNOWN_AGENT_EXIT_CODE = 16
# ---------------------------------------------------------------------------


def test_dispatch_exports_unknown_agent_exit_code() -> None:
    """Taxonomy constant must be exported from dispatch module."""
    from fno.agents import dispatch

    assert hasattr(dispatch, "UNKNOWN_AGENT_EXIT_CODE")
    assert dispatch.UNKNOWN_AGENT_EXIT_CODE == 16


# ---------------------------------------------------------------------------
# _claude_create_path extracted as module-level function
# ---------------------------------------------------------------------------


def test_claude_create_path_is_module_level_function() -> None:
    """Task spec: _claude_create_path must be a module-level helper for Task 1.2."""
    from fno.agents import dispatch

    assert hasattr(dispatch, "_claude_create_path"), (
        "_claude_create_path must be a module-level function so Task 1.2 can call it"
    )
    import inspect
    assert callable(dispatch._claude_create_path)
    # Must be a module-level function (not a closure or lambda)
    assert inspect.isfunction(dispatch._claude_create_path)


# ---------------------------------------------------------------------------
# cmd_ask: kind=="create" branch removed (unreachable after this task)
# ---------------------------------------------------------------------------


def test_cmd_ask_followup_result_prints_reply(tmp_path: Path, monkeypatch) -> None:
    """After this task, cmd_ask only handles kind='followup'. Verify it still works."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents import dispatch as dispatch_mod

    def fake_dispatch_ask(**kwargs):
        return dispatch_mod.DispatchAskResult(
            kind="followup",
            short_id="abc12345",
            reply="reply content here",
        )

    monkeypatch.setattr(dispatch_mod, "dispatch_ask", fake_dispatch_ask)

    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(agents_app, ["ask", "agent-x", "msg"])
    assert result.exit_code == 0
    assert result.stdout == "reply content here"
