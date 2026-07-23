"""Tests for RuntimeAdapter contract and claude-code implementation."""
from __future__ import annotations

import os
import uuid
from unittest.mock import MagicMock, patch

import pytest


# AC1-HP: Adapter registers + dispatches
def test_ac1_hp_get_adapter_claude_code():
    """get_adapter('claude-code') returns a RuntimeAdapter with name='claude-code'."""
    from fno.adapters import get_adapter

    adapter = get_adapter("claude-code")
    assert adapter.name == "claude-code"


def test_ac1_hp_adapter_implements_protocol():
    """claude-code adapter implements all RuntimeAdapter protocol methods."""
    from fno.adapters import get_adapter

    adapter = get_adapter("claude-code")
    assert callable(getattr(adapter, "spawn_worker", None))
    assert callable(getattr(adapter, "create_worktree", None))
    assert callable(getattr(adapter, "call_api", None))
    assert callable(getattr(adapter, "health", None))


def test_ac1_hp_adapter_health_returns_dataclass():
    """health() returns an AdapterHealth with ok and details fields."""
    from fno.adapters import get_adapter
    from fno.adapters.base import AdapterHealth

    adapter = get_adapter("claude-code")
    result = adapter.health()
    assert isinstance(result, AdapterHealth)
    assert isinstance(result.ok, bool)
    assert isinstance(result.details, dict)


def test_ac1_hp_unknown_adapter_raises():
    """get_adapter raises ValueError for unknown adapter names."""
    from fno.adapters import get_adapter

    with pytest.raises(ValueError, match="unknown adapter"):
        get_adapter("nonexistent-adapter")


# AC2-HP: spawn_worker refuses in-session shell spawn
def test_ac2_hp_spawn_worker_refuses_in_session(monkeypatch):
    """When CLAUDECODE_SESSION_ID is set, spawn_worker returns skill_dispatch_required."""
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "test-session-abc123")
    from fno.adapters import get_adapter

    adapter = get_adapter("claude-code")
    result = adapter.spawn_worker(prompt="do something useful")

    assert result["action"] == "skill_dispatch_required"
    assert "next_step" in result
    assert "register-worker" in result["next_step"]


def test_ac2_hp_spawn_worker_does_not_exec_subprocess_in_session(monkeypatch):
    """When CLAUDECODE_SESSION_ID is set, no subprocess is spawned."""
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "test-session-abc123")
    from fno.adapters import get_adapter

    adapter = get_adapter("claude-code")
    with patch("subprocess.Popen") as mock_popen:
        adapter.spawn_worker(prompt="do something")
        mock_popen.assert_not_called()


# AC3-HP: spawn_worker spawns subprocess external
def test_ac3_hp_spawn_worker_external_execs_claude(monkeypatch):
    """When CLAUDECODE_SESSION_ID is unset, spawn_worker execs 'claude -p' subprocess."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    from fno.adapters import get_adapter

    adapter = get_adapter("claude-code")

    fake_proc = MagicMock()
    fake_proc.pid = 99999
    fake_proc.poll.return_value = None  # simulate still-running process
    with patch("subprocess.Popen", return_value=fake_proc) as mock_popen:
        result = adapter.spawn_worker(prompt="build feature X")

    # Verify subprocess.Popen was called with claude -p
    assert mock_popen.called
    call_args = mock_popen.call_args[0][0]  # first positional arg is the command list
    assert call_args[0] == "claude"
    assert "-p" in call_args

    # Verify result shape
    assert "worker_id" in result
    assert result["pid"] == 99999
    assert "started_at" in result


def test_ac3_hp_spawn_worker_external_result_contains_prompt(monkeypatch):
    """External spawn result includes the worker_id as a unique identifier."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    from fno.adapters import get_adapter

    adapter = get_adapter("claude-code")

    fake_proc = MagicMock()
    fake_proc.pid = 12345
    fake_proc.poll.return_value = None  # simulate still-running process
    with patch("subprocess.Popen", return_value=fake_proc):
        result1 = adapter.spawn_worker(prompt="task A")
        result2 = adapter.spawn_worker(prompt="task B")

    # Each spawn should have a unique worker_id
    assert result1["worker_id"] != result2["worker_id"]
