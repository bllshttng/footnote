"""Tests for ClaudeCodeAdapter.

Most adapter behavior is exercised via integration tests in the review
runners; this module pins the create_worktree delegation contract added
when the helper was extracted to ``_shared.py``.
"""
from __future__ import annotations

from unittest import mock

from fno.adapters.claude_code import ClaudeCodeAdapter


def test_create_worktree_delegates_to_shared(tmp_path, monkeypatch):
    """AC1.2-HP: ClaudeCodeAdapter.create_worktree must delegate to _shared.create_worktree."""
    monkeypatch.chdir(tmp_path)

    expected_path = str(tmp_path / ".fno" / "worktrees" / "fno-delegated")
    sentinel = {
        "worktree_path": expected_path,
        "branch": "feature/delegated",
        "status": "created",
    }

    with mock.patch(
        "fno.adapters.claude_code._create_worktree",
        return_value=sentinel,
    ) as mocked:
        adapter = ClaudeCodeAdapter()
        result = adapter.create_worktree(name="delegated")

    mocked.assert_called_once_with(name="delegated", base="main")
    assert result is sentinel


def test_create_worktree_passes_base_through(tmp_path, monkeypatch):
    """Delegation preserves the base= keyword argument."""
    monkeypatch.chdir(tmp_path)

    expected_path = str(tmp_path / ".fno" / "worktrees" / "fno-x")
    with mock.patch(
        "fno.adapters.claude_code._create_worktree",
        return_value={
            "worktree_path": expected_path,
            "branch": "feature/x",
            "status": "created",
        },
    ) as mocked:
        ClaudeCodeAdapter().create_worktree(name="x", base="release/y")

    mocked.assert_called_once_with(name="x", base="release/y")


def test_create_worktree_returns_already_exists_via_delegation(tmp_path, monkeypatch):
    """End-to-end delegation: existing path short-circuits without invoking git."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / ".fno" / "worktrees" / "fno-preexisting").mkdir(parents=True)

    with mock.patch("fno.adapters._shared.subprocess.run") as mock_run, \
         mock.patch(
             "fno.adapters._shared.worktree_path",
             return_value=tmp_path / ".fno" / "worktrees" / "fno-preexisting",
         ):
        result = ClaudeCodeAdapter().create_worktree(name="preexisting")

    assert result["status"] == "already-exists"
    assert result["branch"] == "feature/preexisting"
    mock_run.assert_not_called()


def test_adapter_name_remains_claude_code():
    """The adapter name is part of the registry contract and must not drift."""
    assert ClaudeCodeAdapter.name == "claude-code"


def test_health_reports_in_session_flag(monkeypatch):
    """health() exposes whether the process is inside a Claude Code session."""
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "abc")
    import shutil as real_shutil

    with mock.patch.object(real_shutil, "which", return_value="/usr/local/bin/claude"):
        health = ClaudeCodeAdapter().health()

    assert health.ok is True
    assert health.details["in_session"] is True
