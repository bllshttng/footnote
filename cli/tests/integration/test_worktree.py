"""Integration tests for the surviving ``list_worktrees`` runtime surface.

The create/remove minters are gone (x-93c9); the create/remove suites
went with them. ``HOME`` is pinned to a per-test temp directory so the
worktree bases land inside the sandbox.
"""
from __future__ import annotations

import subprocess
import uuid
from pathlib import Path

import pytest


_PROJECT_ID = "testproj"


@pytest.fixture
def tmp_git_repo(tmp_path: Path, monkeypatch):
    """Create a minimal git repo in tmp_path with an initial commit on ``main``.

    Pins HOME to a per-test temp dir so the canonical worktree base
    (~/.fno/worktrees/) lands inside the sandbox. Writes a tiny
    ``.fno/settings.yaml`` declaring ``project.id`` so worktree
    paths are deterministic.
    """
    monkeypatch.setenv("HOME", str(tmp_path))
    subprocess.run(
        ["git", "init", "-b", "main", str(tmp_path)],
        check=True, capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "config", "user.name", "Test User"],
        cwd=tmp_path, check=True, capture_output=True
    )
    readme = tmp_path / "README.md"
    readme.write_text("# Test Repo\n")
    subprocess.run(["git", "add", "README.md"], cwd=tmp_path, check=True, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", "initial"],
        cwd=tmp_path, check=True, capture_output=True
    )
    # Declare a stable project id so worktree path resolution is deterministic.
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(exist_ok=True)
    (fno_dir / "config.toml").write_text(
        f'[project]\nid = "{_PROJECT_ID}"\n', encoding="utf-8",
    )
    return tmp_path


def _unique_name() -> str:
    return "test-" + uuid.uuid4().hex[:8]


def test_list_worktrees_includes_legacy_base_during_transition(tmp_git_repo):
    """``list_worktrees`` surfaces worktrees at the legacy ``.claude/worktrees/`` base.

    Pinned by integration-test-analyzer gap finding: ``runtime.list_worktrees``
    accepts both the canonical ``~/.fno/worktrees/`` and the legacy
    ``<repo>/.claude/worktrees/`` bases through the transition window.
    Without this regression test the dual-base branch could silently regress
    to canonical-only and operators on in-flight legacy worktrees would lose
    `list` visibility.
    """
    from fno.runtime.worktree import list_worktrees

    name = _unique_name()
    legacy_path = tmp_git_repo / ".claude" / "worktrees" / name
    legacy_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        # Create a real legacy worktree via git so `git worktree list` reports it.
        subprocess.run(
            ["git", "worktree", "add", "-b", f"feature/{name}", str(legacy_path), "main"],
            cwd=tmp_git_repo, check=True, capture_output=True,
        )

        result = list_worktrees(repo_root=tmp_git_repo)

        paths = [w["worktree_path"] for w in result]
        assert str(legacy_path) in paths, (
            f"legacy worktree at {legacy_path} not reported by list_worktrees: {paths}"
        )
        # Pick the legacy entry and verify its shape
        legacy_entry = next(w for w in result if w["worktree_path"] == str(legacy_path))
        assert legacy_entry["branch"] == f"feature/{name}"
        assert legacy_entry["name"] == name
    finally:
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(legacy_path)],
            cwd=tmp_git_repo, capture_output=True,
        )
        subprocess.run(
            ["git", "branch", "-D", f"feature/{name}"],
            cwd=tmp_git_repo, capture_output=True,
        )
