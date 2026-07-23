"""Tests for FNO_AGENT_* env injection on provider subprocess spawn.

Task 2.2 from 2026-05-22-fno-agents-observability.md.

Each provider's create() path must inject:
- FNO_AGENT_SELF=<agent name>
- FNO_AGENT_PROVIDER=<provider name>

into the spawned subprocess's environment so nested `fno agents ask`
calls from inside that agent attribute back to this parent via the
caller_kind=nested_agent branch in context.build_context().

FNO_AGENT_SESSION is intentionally omitted on create — the session
id is not known until the subprocess returns. The nested-attribution
path handles its absence as caller_kind=nested_agent + from_session_id=None.
"""
from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict

import pytest


# ---------------------------------------------------------------------------
# claude.bg_create
# ---------------------------------------------------------------------------


def test_claude_bg_create_injects_agent_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """claude.bg_create passes env containing FNO_AGENT_SELF/PROVIDER."""
    from fno.agents.providers import claude as claude_mod

    captured: Dict[str, Any] = {}

    def fake_run(argv: list[str], **kw: Any) -> SimpleNamespace:
        captured["argv"] = argv
        captured["env"] = kw.get("env")
        # Stdout shape must match Locked Decision 6: backgrounded · <8hex> · ...
        return SimpleNamespace(
            returncode=0,
            stdout="backgrounded \xb7 abc12345 \xb7 ok\n",
            stderr="",
        )

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)

    claude_mod.bg_create(name="alpha", message="hi", cwd=tmp_path)

    env = captured["env"]
    assert env is not None, "expected env kwarg to subprocess.run"
    assert env["FNO_AGENT_SELF"] == "alpha"
    assert env["FNO_AGENT_PROVIDER"] == "claude"
    # Parent env must be preserved (sample check on PATH)
    assert "PATH" in env, "spawn env should inherit parent PATH"


# ---------------------------------------------------------------------------
# codex.create -> _run_codex
# ---------------------------------------------------------------------------


def test_codex_create_threads_agent_self_to_run_codex(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """codex.create forwards agent_self into _run_codex's spawn env."""
    from fno.agents.providers import codex as codex_mod

    captured: Dict[str, Any] = {}

    def fake_popen(argv: list[str], **kw: Any) -> Any:
        captured["argv"] = argv
        captured["env"] = kw.get("env")
        raise codex_mod.CodexInvocationError(1)

    monkeypatch.setattr(codex_mod, "_subprocess_popen", fake_popen)

    output_path = tmp_path / "out.jsonl"
    output_path.touch()

    with pytest.raises(codex_mod.CodexInvocationError):
        codex_mod.create(
            cwd=tmp_path,
            prompt="hello",
            from_name="orchestrator",
            yolo=False,
            output_path=output_path,
            agent_self="beta",
        )

    env = captured["env"]
    assert env is not None
    assert env["FNO_AGENT_SELF"] == "beta"
    assert env["FNO_AGENT_PROVIDER"] == "codex"
    assert "PATH" in env


def test_codex_create_without_agent_self_skips_injection(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When agent_self=None, env=None so the child inherits parent env unchanged."""
    from fno.agents.providers import codex as codex_mod

    captured: Dict[str, Any] = {}

    def fake_popen(argv: list[str], **kw: Any) -> Any:
        captured["env"] = kw.get("env")
        raise codex_mod.CodexInvocationError(1)

    monkeypatch.setattr(codex_mod, "_subprocess_popen", fake_popen)

    output_path = tmp_path / "out.jsonl"
    output_path.touch()

    with pytest.raises(codex_mod.CodexInvocationError):
        codex_mod.create(
            cwd=tmp_path,
            prompt="hello",
            from_name="orchestrator",
            yolo=False,
            output_path=output_path,
            # agent_self omitted
        )

    assert captured["env"] is None


# ---------------------------------------------------------------------------
# Failure-recovery / degradation contract
# ---------------------------------------------------------------------------


def test_env_session_intentionally_absent_on_create(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Locked spec: FNO_AGENT_SESSION is unset on create (not known yet).

    A nested `fno agents ask` from inside the spawned agent will see
    caller_kind=nested_agent (because SELF is set) but from_session_id=None
    (because SESSION is absent). This is the AC3-ERR-adjacent graceful
    degradation path.
    """
    from fno.agents.providers import claude as claude_mod

    captured: Dict[str, Any] = {}

    def fake_run(argv: list[str], **kw: Any) -> SimpleNamespace:
        captured["env"] = kw.get("env")
        return SimpleNamespace(
            returncode=0,
            stdout="backgrounded \xb7 deadbeef \xb7 ok\n",
            stderr="",
        )

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)

    claude_mod.bg_create(name="delta", message="hi", cwd=tmp_path)

    env = captured["env"]
    assert "FNO_AGENT_SELF" in env
    assert "FNO_AGENT_SESSION" not in env
