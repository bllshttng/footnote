"""Integration tests for 'fno runtime worktree' subcommand.

Uses a temporary git repository fixture so we never touch real worktrees.
``HOME`` is pinned to a per-test temp directory so worktrees that land at
the canonical ``~/.fno/worktrees/`` location don't escape the
sandbox (Plan ab-3180b3f4 path standardisation).
"""
from __future__ import annotations

import json
import subprocess
import uuid
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


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


def _canonical_wt(repo: Path, name: str) -> Path:
    """Canonical worktree path under the pinned HOME (== repo for these tests)."""
    return repo / ".fno" / "worktrees" / f"{_PROJECT_ID}-{name}"


def _cleanup_worktree(repo: Path, name: str) -> None:
    """Remove worktree and branch created during a test."""
    wt_path = _canonical_wt(repo, name)
    branch = f"feature/{name}"
    subprocess.run(
        ["git", "worktree", "remove", "--force", str(wt_path)],
        cwd=repo, capture_output=True
    )
    subprocess.run(
        ["git", "branch", "-D", branch],
        cwd=repo, capture_output=True
    )


# AC1-HP: worktree create makes a worktree + branch + symlink
def test_ac1_hp_worktree_create(tmp_git_repo):
    """worktree create makes ~/.fno/worktrees/{proj}-{name}, branch feature/{name}, .fno symlink."""
    name = _unique_name()
    try:
        from fno.runtime.worktree import create_worktree

        result = create_worktree(name=name, base="main", repo_root=tmp_git_repo)

        assert result["status"] == "created"
        wt_path = _canonical_wt(tmp_git_repo, name)
        assert wt_path.exists(), f"worktree dir should exist at {wt_path}"

        # Check branch is feature/{name}
        branch_result = subprocess.run(
            ["git", "branch", "--show-current"],
            cwd=wt_path, capture_output=True, text=True
        )
        assert branch_result.stdout.strip() == f"feature/{name}"

        # Check .fno symlink
        fno_link = wt_path / ".fno"
        assert fno_link.is_symlink(), ".fno should be a symlink in the worktree"

        assert result["worktree_path"] == str(wt_path)
        assert result["branch"] == f"feature/{name}"
    finally:
        _cleanup_worktree(tmp_git_repo, name)


# AC2-EDGE: idempotent create returns existing
def test_ac2_edge_worktree_create_idempotent(tmp_git_repo):
    """Second create on an existing worktree returns already-exists, not an error."""
    name = _unique_name()
    try:
        from fno.runtime.worktree import create_worktree

        # First create
        r1 = create_worktree(name=name, base="main", repo_root=tmp_git_repo)
        assert r1["status"] == "created"

        # Second create - must be idempotent
        r2 = create_worktree(name=name, base="main", repo_root=tmp_git_repo)
        assert r2["status"] == "already-exists"
        assert "worktree_path" in r2
    finally:
        _cleanup_worktree(tmp_git_repo, name)


def test_ac7_legacy_path_short_circuits_create(tmp_git_repo):
    """AC7 back-compat: an existing ``.claude/worktrees/{name}/`` dir is treated as already-exists."""
    from fno.runtime.worktree import create_worktree

    name = _unique_name()
    legacy = tmp_git_repo / ".claude" / "worktrees" / name
    legacy.mkdir(parents=True)

    result = create_worktree(name=name, base="main", repo_root=tmp_git_repo)

    assert result["status"] == "already-exists"
    assert result["worktree_path"] == str(legacy)


# AC3-HP: worktree remove cleans up
def test_ac3_hp_worktree_remove(tmp_git_repo):
    """worktree remove deletes the worktree directory."""
    name = _unique_name()
    from fno.runtime.worktree import create_worktree, remove_worktree

    create_worktree(name=name, base="main", repo_root=tmp_git_repo)
    wt_path = _canonical_wt(tmp_git_repo, name)
    assert wt_path.exists()

    result = remove_worktree(name=name, repo_root=tmp_git_repo, prune_branch=False)

    assert result["status"] == "removed"
    assert not wt_path.exists(), "worktree dir should be gone after remove"

    # Branch should still exist (prune_branch=False)
    branch_check = subprocess.run(
        ["git", "branch", "--list", f"feature/{name}"],
        cwd=tmp_git_repo, capture_output=True, text=True
    )
    assert f"feature/{name}" in branch_check.stdout

    # Cleanup the branch
    subprocess.run(["git", "branch", "-D", f"feature/{name}"], cwd=tmp_git_repo, capture_output=True)


def test_ac3_hp_worktree_remove_prune_branch(tmp_git_repo):
    """worktree remove with --prune-branch deletes both worktree and branch."""
    name = _unique_name()
    from fno.runtime.worktree import create_worktree, remove_worktree

    create_worktree(name=name, base="main", repo_root=tmp_git_repo)

    remove_worktree(name=name, repo_root=tmp_git_repo, prune_branch=True)

    wt_path = _canonical_wt(tmp_git_repo, name)
    assert not wt_path.exists()

    branch_check = subprocess.run(
        ["git", "branch", "--list", f"feature/{name}"],
        cwd=tmp_git_repo, capture_output=True, text=True
    )
    assert branch_check.stdout.strip() == "", "branch should be deleted when prune_branch=True"


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


def test_remove_worktree_accepts_full_canonical_name_from_list(tmp_git_repo):
    """``remove_worktree`` can take the full ``{proj}-{slug}`` name from ``list_worktrees``.

    Pinned by Gemini HIGH PR #234. The earlier code computed
    ``worktree_path(name=...)`` first which always prepended the project
    id, so ``remove_worktree(name="testproj-foo")`` looked for
    ``~/.fno/worktrees/testproj-testproj-foo/`` and silently
    fell through to legacy lookup. Now full canonical names round-trip
    through list -> remove without re-prefixing.
    """
    from fno.runtime.worktree import create_worktree, list_worktrees, remove_worktree

    slug = _unique_name()
    full_name = f"{_PROJECT_ID}-{slug}"
    try:
        create_worktree(name=slug, base="main", repo_root=tmp_git_repo)
        canonical = _canonical_wt(tmp_git_repo, slug)
        assert canonical.exists()

        # Verify list emits the full name (this is what callers will pass back to remove)
        listed = list_worktrees(repo_root=tmp_git_repo)
        names = [w["name"] for w in listed]
        assert full_name in names, f"list should emit full name; got {names}"

        # Remove via the FULL canonical name (not the slug)
        result = remove_worktree(name=full_name, repo_root=tmp_git_repo, prune_branch=False)

        assert result["status"] == "removed"
        assert result["worktree_path"] == str(canonical), (
            f"remove should resolve full name to canonical path; got {result['worktree_path']}"
        )
        assert not canonical.exists()
    finally:
        subprocess.run(
            ["git", "branch", "-D", f"feature/{slug}"],
            cwd=tmp_git_repo, capture_output=True,
        )


def test_remove_worktree_falls_back_to_legacy_path_when_canonical_missing(tmp_git_repo):
    """``remove_worktree`` operates on the legacy path when canonical doesn't exist.

    Pins the AC7 back-compat fallback in ``runtime.remove_worktree``: when
    the canonical ``~/.fno/worktrees/{proj}-{name}/`` is absent but a
    legacy ``<repo>/.claude/worktrees/{name}/`` exists, git is invoked
    against the legacy directory. Without this test a regression could drop
    the fallback branch silently.
    """
    from fno.runtime.worktree import remove_worktree

    name = _unique_name()
    legacy_path = tmp_git_repo / ".claude" / "worktrees" / name
    legacy_path.parent.mkdir(parents=True, exist_ok=True)
    # Create a real legacy worktree so git knows it
    subprocess.run(
        ["git", "worktree", "add", "-b", f"feature/{name}", str(legacy_path), "main"],
        cwd=tmp_git_repo, check=True, capture_output=True,
    )

    canonical = _canonical_wt(tmp_git_repo, name)
    assert not canonical.exists(), "canonical path must NOT exist for fallback test"
    assert legacy_path.exists()

    result = remove_worktree(name=name, repo_root=tmp_git_repo, prune_branch=True)

    assert result["status"] == "removed"
    assert result["worktree_path"] == str(legacy_path), (
        f"remove should operate on the legacy path; got {result['worktree_path']}"
    )
    assert not legacy_path.exists(), "legacy worktree should be gone after remove"
    assert result.get("branch_pruned") is True


# CLI integration via typer runner
def test_ac1_hp_worktree_cli_create(tmp_git_repo, monkeypatch):
    """fno runtime worktree --action create --name X works end-to-end."""
    monkeypatch.chdir(tmp_git_repo)

    name = _unique_name()
    try:
        result = runner.invoke(
            app,
            ["runtime", "worktree", "--action", "create", "--name", name, "--json"],
        )
        assert result.exit_code == 0, f"Output: {result.output}"
        data = json.loads(result.output)
        assert data["status"] in ("created", "already-exists")
        assert "worktree_path" in data
    finally:
        _cleanup_worktree(tmp_git_repo, name)
