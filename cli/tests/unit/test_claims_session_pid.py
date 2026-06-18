"""Unit tests for fno.claims.session_pid: durable session pid resolution."""
from __future__ import annotations

import os
from unittest.mock import patch

import psutil
import pytest

from fno.claims.session_pid import resolve_session_pid


class _FakeProc:
    """Minimal psutil.Process stand-in for a chosen ancestor chain."""

    def __init__(self, pid, name, exe, parent=None):
        self.pid = pid
        self._name = name
        self._exe = exe
        self._parent = parent

    def name(self):
        return self._name

    def exe(self):
        return self._exe

    def parent(self):
        return self._parent


def _chain(*specs):
    """Build a child->...->root chain from (pid, name, exe) specs (child first)."""
    parent = None
    for pid, name, exe in reversed(specs):
        parent = _FakeProc(pid, name, exe, parent=parent)
    # walk back to the first (child) node
    node = parent
    nodes = []
    while node is not None:
        nodes.append(node)
        node = node._parent
    return nodes[0]


def test_env_override_wins_when_alive():
    """FNO_SESSION_PID set to a live pid is returned verbatim (launcher hook)."""
    with patch.dict(os.environ, {"FNO_SESSION_PID": str(os.getpid())}):
        assert resolve_session_pid() == os.getpid()


def test_env_override_ignored_when_dead():
    """A dead/malformed FNO_SESSION_PID falls through to the walk."""
    dead = 999_999
    while psutil.pid_exists(dead):
        dead += 1
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "2.1.177", "/Users/x/.local/share/claude/versions/2.1.177"),
    )
    with patch.dict(os.environ, {"FNO_SESSION_PID": str(dead)}):
        with patch("psutil.Process", return_value=child):
            assert resolve_session_pid(from_pid=10) == 20


def test_env_override_malformed_falls_through():
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "claude", "/Users/x/.local/bin/claude"),
    )
    with patch.dict(os.environ, {"FNO_SESSION_PID": "not-a-pid"}):
        with patch("psutil.Process", return_value=child):
            assert resolve_session_pid(from_pid=10) == 20


def test_walk_finds_versioned_binary_by_exe_path():
    """The versioned binary's name() is a version string; match on exe path."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (15, "node", "/usr/bin/node"),
        (20, "2.1.177", "/Users/x/.local/share/claude/versions/2.1.177"),
    )
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("FNO_SESSION_PID", None)
        with patch("psutil.Process", return_value=child):
            assert resolve_session_pid(from_pid=10) == 20


def test_walk_returns_nearest_claude_ancestor():
    """When several claude ancestors exist, the NEAREST is returned."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "claude", "/Users/x/.local/bin/claude"),            # nearest
        (30, "2.1.177", "/Users/x/.local/share/claude/versions/2.1.177"),
    )
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("FNO_SESSION_PID", None)
        with patch("psutil.Process", return_value=child):
            assert resolve_session_pid(from_pid=10) == 20


def test_walk_returns_none_when_no_claude_ancestor():
    """No claude ancestor -> None -> caller degrades to TTL-only."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "node", "/usr/bin/node"),
        (30, "zsh", "/bin/zsh"),
    )
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("FNO_SESSION_PID", None)
        with patch("psutil.Process", return_value=child):
            assert resolve_session_pid(from_pid=10) is None


def test_walk_returns_none_when_start_pid_gone():
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("FNO_SESSION_PID", None)
        with patch("psutil.Process", side_effect=psutil.NoSuchProcess(123)):
            assert resolve_session_pid(from_pid=123) is None


def test_walk_returns_none_on_negative_start_pid():
    """A negative/zero start pid raises ValueError in psutil.Process; degrade to None."""
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("FNO_SESSION_PID", None)
        with patch("psutil.Process", side_effect=ValueError("invalid pid")):
            assert resolve_session_pid(from_pid=-1) is None


def test_env_override_non_positive_ignored():
    """FNO_SESSION_PID of 0 or -1 must not be honored even if pid_exists is True."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "claude", "/Users/x/.local/bin/claude"),
    )
    with patch.dict(os.environ, {"FNO_SESSION_PID": "0"}):
        with patch("psutil.pid_exists", return_value=True):
            with patch("psutil.Process", return_value=child):
                # 0 is rejected -> walk runs -> finds the claude ancestor.
                assert resolve_session_pid(from_pid=10) == 20
