"""Unit tests for fno.claims.session_pid: durable session pid resolution."""
from __future__ import annotations

import os
from unittest.mock import patch

import psutil
import pytest

from fno.claims.session_pid import resolve_session_pid


class _FakeProc:
    """Minimal psutil.Process stand-in for a chosen ancestor chain.

    ``name``/``exe``/``cmdline`` may each be an exception INSTANCE, in which case
    the corresponding getter raises it (models a per-getter psutil failure).
    ``cmdline`` defaults to ``[exe]`` so plain (pid, name, exe) specs behave like
    a real process without a bespoke argv.
    """

    def __init__(self, pid, name, exe, parent=None, cmdline=None):
        self.pid = pid
        self._name = name
        self._exe = exe
        self._parent = parent
        self._cmdline = [exe] if cmdline is None else cmdline

    def _get(self, value):
        if isinstance(value, BaseException):
            raise value
        return value

    def name(self):
        return self._get(self._name)

    def exe(self):
        return self._get(self._exe)

    def cmdline(self):
        return self._get(self._cmdline)

    def parent(self):
        return self._parent


def _chain(*specs):
    """Build a child->...->root chain (child first). Each spec is
    (pid, name, exe) or (pid, name, exe, cmdline)."""
    parent = None
    for spec in reversed(specs):
        pid, name, exe = spec[0], spec[1], spec[2]
        cmdline = spec[3] if len(spec) > 3 else None
        parent = _FakeProc(pid, name, exe, parent=parent, cmdline=cmdline)
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


# --- non-claude harness resolution (x-5e58) -----------------------------------


def _resolve_from(child, from_pid):
    """Run resolve_session_pid over a fake chain with FNO_SESSION_PID unset."""
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("FNO_SESSION_PID", None)
        with patch("psutil.Process", return_value=child):
            return resolve_session_pid(from_pid=from_pid)


@pytest.mark.parametrize("token", ["codex", "opencode", "agy"])
def test_native_binary_harness_resolves_by_basename(token):
    """Native-binary harnesses have their name as the exe basename."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, token, f"/opt/homebrew/bin/{token}"),
    )
    assert _resolve_from(child, 10) == 20


def test_node_shim_gemini_resolves_via_cmdline_symlink():
    """gemini is a node shim: name()==node, exe()==node, cmdline()[1]==bin/gemini."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "node", "/usr/bin/node", ["node", "/Users/x/.gemini/bin/gemini"]),
    )
    assert _resolve_from(child, 10) == 20


def test_node_shim_gemini_resolves_via_cmdline_resolved_script():
    """The resolved-script argv shape (gemini.js) matches via the stem rule."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "node", "/usr/bin/node", ["node", "/Users/x/lib/gemini-cli/gemini.js"]),
    )
    assert _resolve_from(child, 10) == 20


def test_substring_trap_legacy_does_not_match_agy():
    """`agy` is a substring of `legacy`; segment-exact matching must not match it."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "tool", "/opt/legacy/bin/tool", ["/opt/legacy/bin/tool", "--run"]),
    )
    assert _resolve_from(child, 10) is None


def test_substring_trap_codex_framework_does_not_match_codex():
    """The ChatGPT app's `Codex Framework.framework` segments are not `codex`."""
    exe = (
        "/Applications/ChatGPT.app/Contents/Frameworks/"
        "Codex Framework.framework/Versions/A/helper"
    )
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "helper", exe, [exe]),
    )
    assert _resolve_from(child, 10) is None


def test_nearest_harness_wins_across_mixed_chain():
    """A claude worker nested under codex anchors the NEAREST harness (claude)."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "claude", "/Users/x/.local/bin/claude"),   # nearest
        (30, "codex", "/opt/homebrew/bin/codex"),        # outer
    )
    assert _resolve_from(child, 10) == 20


def test_cmdline_access_denied_degrades_to_other_getters():
    """cmdline() raising AccessDenied still lets name()/exe() resolve the harness."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        (20, "codex", "/opt/homebrew/bin/codex", psutil.AccessDenied(20)),
    )
    assert _resolve_from(child, 10) == 20


def test_cmdline_zombie_on_only_source_continues_walk():
    """A node-shim ancestor whose cmdline() zombies yields no match; walk continues."""
    child = _chain(
        (10, "bash", "/bin/bash"),
        # name/exe say only "node"; its one identifying source (cmdline) zombies.
        (20, "node", "/usr/bin/node", psutil.ZombieProcess(20)),
        (30, "codex", "/opt/homebrew/bin/codex"),
    )
    assert _resolve_from(child, 10) == 30
