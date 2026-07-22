"""Tests for graph._intake._git_repo_root.

`_git_repo_root` now delegates the worktree-list parsing to the shared
`paths.resolve_canonical_worktree` (covered by test_resolve_canonical_worktree.py);
here we test only the delegation + the `--show-toplevel` fallback when the
helper finds no usable working tree (bare-only / separate-git-dir).
(ab-91a004af worktree-resolution; originally ab-d6cf58be.)
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest


def test_returns_helper_result_normalized(monkeypatch: pytest.MonkeyPatch) -> None:
    """When the shared helper resolves a canonical working tree, return it
    normalized to a string (the durable backlog cwd)."""
    from fno.graph import _intake

    monkeypatch.setattr(
        _intake._paths, "resolve_canonical_worktree", lambda *a, **k: Path("/repos/fno")
    )
    assert _intake._git_repo_root() == os.path.normpath("/repos/fno")


def test_falls_back_to_show_toplevel_when_helper_none(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Helper returns None (bare-only / separate-git-dir with no linked
    checkout) -> fall back to `git rev-parse --show-toplevel`."""
    from fno.graph import _intake

    monkeypatch.setattr(_intake._paths, "resolve_canonical_worktree", lambda *a, **k: None)
    monkeypatch.setattr(
        _intake.subprocess, "check_output", lambda *a, **k: "/some/checkout\n"
    )
    assert _intake._git_repo_root() == os.path.normpath("/some/checkout")


def test_returns_none_when_helper_none_and_not_git(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Helper None and `--show-toplevel` fails (not a repo) -> None."""
    from fno.graph import _intake

    monkeypatch.setattr(_intake._paths, "resolve_canonical_worktree", lambda *a, **k: None)

    def _boom(*_a: object, **_k: object) -> str:
        raise subprocess.CalledProcessError(128, ["git", "rev-parse", "--show-toplevel"])

    monkeypatch.setattr(_intake.subprocess, "check_output", _boom)
    assert _intake._git_repo_root() is None
