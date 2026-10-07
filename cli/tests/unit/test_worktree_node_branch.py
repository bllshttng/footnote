"""The `_node_branch` fallback contract (x-93c9).

The helper shells the one Rust resolver through
`fno backlog get <name> --field _branch`. Anything but a clean non-null
answer degrades to today's ``feature/<name>``: a deployed binary that
predates ``_branch`` prints ``null``, a non-node name exits 1, a slow
binary times out. ``subprocess.run`` is patched, so no real binary runs.
"""
from __future__ import annotations

import subprocess

from fno.worktree_cli import cli as wt_cli


class _Result:
    def __init__(self, returncode=0, stdout=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = ""


def _patch(monkeypatch, result):
    calls = []

    def fake_run(cmd, **kwargs):
        calls.append((cmd, kwargs))
        if isinstance(result, Exception):
            raise result
        return result

    monkeypatch.setattr(wt_cli.subprocess, "run", fake_run)
    return calls


def test_a_printed_branch_is_used(monkeypatch):
    calls = _patch(monkeypatch, _Result(0, "bugfix/x-aaaa-wrong-close\n"))
    assert wt_cli._node_branch("x-aaaa") == "bugfix/x-aaaa-wrong-close"
    (cmd, kwargs), = calls
    assert cmd[-5:] == ["backlog", "get", "x-aaaa", "--field", "_branch"]
    assert kwargs["timeout"] == 30


def test_null_falls_back_to_feature_name(monkeypatch):
    _patch(monkeypatch, _Result(0, "null\n"))
    assert wt_cli._node_branch("x-aaaa") == "feature/x-aaaa"


def test_a_timeout_falls_back(monkeypatch):
    _patch(
        monkeypatch,
        subprocess.TimeoutExpired(cmd="fno", timeout=30),
    )
    assert wt_cli._node_branch("x-aaaa") == "feature/x-aaaa"
