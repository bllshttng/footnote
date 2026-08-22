"""Integration tests for fno worktree status/cleanup/archive in a foreign repo (x-05d7).

Asserts that all three verbs resolve scripts/lib/worktree-lifecycle.sh from the
plugin root (CLAUDE_PLUGIN_ROOT, CODEX_PLUGIN_ROOT, ~/.fno/plugin-root, package root)
rather than failing when executed in a scratch git repo that is not footnote.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def foreign_git_repo(tmp_path: Path) -> Path:
    """Create a minimal scratch git repository that is NOT footnote."""
    repo = tmp_path / "foreign-repo"
    repo.mkdir(parents=True)
    subprocess.run(["git", "init", "-b", "main", str(repo)], check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "test@test.com"], cwd=repo, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "Test User"], cwd=repo, check=True, capture_output=True)
    (repo / "README.md").write_text("# Scratch Repo\n", encoding="utf-8")
    subprocess.run(["git", "add", "README.md"], cwd=repo, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "initial commit"], cwd=repo, check=True, capture_output=True)
    assert not (repo / "scripts" / "lib" / "worktree-lifecycle.sh").exists()
    return repo


@pytest.fixture
def plugin_root() -> Path:
    """Root of the footnote repository that carries the actual plugin scripts."""
    # From cli/tests/integration -> footnote repo root
    return Path(__file__).resolve().parents[3]


def test_worktree_status_in_foreign_repo(foreign_git_repo: Path, plugin_root: Path, monkeypatch, capfd):
    """fno worktree status resolves worktree-lifecycle.sh from the plugin in a foreign repo."""
    monkeypatch.chdir(foreign_git_repo)
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(plugin_root))

    result = runner.invoke(app, ["worktree", "status", "--json"])
    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    captured = capfd.readouterr()
    stdout = captured.out or result.output
    assert "worktree-lifecycle script not found" not in stdout
    assert "worktree-lifecycle script not found" not in captured.err
    data = json.loads(stdout)
    assert "summary" in data


def test_worktree_cleanup_in_foreign_repo(foreign_git_repo: Path, plugin_root: Path, monkeypatch, capfd):
    """fno worktree cleanup resolves worktree-lifecycle.sh from the plugin in a foreign repo."""
    monkeypatch.chdir(foreign_git_repo)
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(plugin_root))

    result = runner.invoke(app, ["worktree", "cleanup", "--dry-run"])
    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    captured = capfd.readouterr()
    assert "worktree-lifecycle script not found" not in (captured.err + captured.out + result.output)


def test_worktree_archive_in_foreign_repo(foreign_git_repo: Path, plugin_root: Path, monkeypatch, capfd):
    """fno worktree archive resolves worktree-lifecycle.sh from the plugin in a foreign repo."""
    monkeypatch.chdir(foreign_git_repo)
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(plugin_root))

    # Add a worktree in the foreign repo under .claude/worktrees/test-archive
    wt_dir = foreign_git_repo / ".claude" / "worktrees" / "test-archive"
    wt_dir.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "worktree", "add", "-b", "feature/test-archive", str(wt_dir), "main"],
        cwd=foreign_git_repo,
        check=True,
        capture_output=True,
    )
    assert wt_dir.exists()

    result = runner.invoke(app, ["worktree", "archive", "test-archive"])
    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    captured = capfd.readouterr()
    assert "worktree-lifecycle script not found" not in (captured.err + captured.out + result.output)
    assert not wt_dir.exists()


def test_worktree_verbs_resolve_via_codex_plugin_root(foreign_git_repo: Path, plugin_root: Path, monkeypatch, capfd):
    """fno worktree verbs resolve via CODEX_PLUGIN_ROOT."""
    monkeypatch.chdir(foreign_git_repo)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    monkeypatch.setenv("CODEX_PLUGIN_ROOT", str(plugin_root))

    result = runner.invoke(app, ["worktree", "status", "--json"])
    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    captured = capfd.readouterr()
    stdout = captured.out or result.output
    assert "worktree-lifecycle script not found" not in (captured.err + stdout)
    data = json.loads(stdout)
    assert "summary" in data


def test_worktree_verbs_resolve_via_persisted_pointer(foreign_git_repo: Path, plugin_root: Path, tmp_path: Path, monkeypatch, capfd):
    """fno worktree verbs resolve via persisted ~/.fno/plugin-root pointer."""
    monkeypatch.chdir(foreign_git_repo)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.delenv("CODEX_PLUGIN_ROOT", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    fno_home = tmp_path / "custom-fno-home"
    fno_home.mkdir(parents=True, exist_ok=True)
    (fno_home / "plugin-root").write_text(str(plugin_root) + "\n", encoding="utf-8")
    monkeypatch.setenv("FNO_HOME", str(fno_home))

    result = runner.invoke(app, ["worktree", "status", "--json"])
    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    captured = capfd.readouterr()
    stdout = captured.out or result.output
    assert "worktree-lifecycle script not found" not in (captured.err + stdout)
    data = json.loads(stdout)
    assert "summary" in data
