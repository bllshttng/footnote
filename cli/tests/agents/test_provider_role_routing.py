"""bg_create role -> spawn-env routing (x-d2fe).

The spawn-env builder is the single hook point for per-spawn model routing.
These tests pin the two ends of the contract at the provider boundary:

- AC1-HP   role=consolidate + key -> spawn env carries the z.ai overrides
           and drops a stale ANTHROPIC_API_KEY so the z.ai token is used.
- AC2-INV  no role -> spawn env is byte-for-byte today's behavior (no
           ANTHROPIC_* routing keys added). Regression guard.
"""
from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict

import pytest


def _fake_run(captured: Dict[str, Any]):
    def fake_run(argv: list[str], **kw: Any) -> SimpleNamespace:
        captured["argv"] = argv
        captured["env"] = kw.get("env")
        return SimpleNamespace(
            returncode=0,
            stdout="backgrounded \xb7 abc12345 \xb7 ok\n",
            stderr="",
        )

    return fake_run


def test_cheap_role_merges_zai_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setenv("ZAI_API_KEY", "zk-secret")
    # A stale Anthropic key in the parent env must be cleared on a route so
    # the z.ai auth token is the one that wins.
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-stale")

    captured: Dict[str, Any] = {}
    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake_run(captured))

    claude_mod.bg_create(
        name="dreamer", message="hi", cwd=tmp_path, role="consolidate"
    )

    env = captured["env"]
    assert env["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert env["ANTHROPIC_AUTH_TOKEN"] == "zk-secret"
    assert env["ANTHROPIC_MODEL"] == "glm-5.1"
    assert "ANTHROPIC_API_KEY" not in env  # stale key cleared on route
    # FNO_AGENT_* injection still happens alongside the route.
    assert env["FNO_AGENT_SELF"] == "dreamer"


def test_no_role_leaves_env_unrouted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setenv("ZAI_API_KEY", "zk-secret")
    monkeypatch.delenv("ANTHROPIC_BASE_URL", raising=False)

    captured: Dict[str, Any] = {}
    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake_run(captured))

    claude_mod.bg_create(name="builder", message="hi", cwd=tmp_path)

    env = captured["env"]
    # Regression guard: the default (no-role) path adds no routing keys.
    assert "ANTHROPIC_BASE_URL" not in env
    assert "ANTHROPIC_AUTH_TOKEN" not in env
    assert "ANTHROPIC_MODEL" not in env
    assert env["FNO_AGENT_SELF"] == "builder"


def test_production_role_leaves_env_unrouted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setenv("ZAI_API_KEY", "zk-secret")

    captured: Dict[str, Any] = {}
    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake_run(captured))

    claude_mod.bg_create(
        name="impl", message="hi", cwd=tmp_path, role="implement"
    )

    env = captured["env"]
    assert "ANTHROPIC_BASE_URL" not in env
    assert "ANTHROPIC_MODEL" not in env


def test_cheap_role_without_key_falls_back_unrouted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents.providers import claude as claude_mod

    monkeypatch.delenv("ZAI_API_KEY", raising=False)

    captured: Dict[str, Any] = {}
    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake_run(captured))

    # Must not raise; spawn still succeeds on the default model.
    result = claude_mod.bg_create(
        name="dreamer", message="hi", cwd=tmp_path, role="consolidate"
    )
    assert result.session_id_out == "abc12345"

    env = captured["env"]
    assert "ANTHROPIC_BASE_URL" not in env
