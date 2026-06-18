"""Integration tests for paths.resolve_canonical_worktree.

The helper resolves the canonical main working tree from `git worktree list
--porcelain`, skipping bare repos and separate-git-dir gitdir mis-reports.
These need REAL git layouts (the gitdir/working-tree distinction is a
filesystem fact - a `.git` child exists for a working tree, not a gitdir), so
the tests build real repos under tmp_path. (ab-91a004af / ab-b66798f7
worktree-resolution design.)
"""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

pytestmark = pytest.mark.skipif(shutil.which("git") is None, reason="git required")


def _git(*args: str, cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-c", "user.email=t@t", "-c", "user.name=t", *args],
        cwd=str(cwd), capture_output=True, text=True, check=True,
    )


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git("init", "-q", cwd=path)
    _git("commit", "--allow-empty", "-qm", "init", cwd=path)


def test_normal_repo_returns_working_tree(tmp_path: Path) -> None:
    from fno.paths import resolve_canonical_worktree

    main = tmp_path / "main"
    _init_repo(main)
    result = resolve_canonical_worktree(main)
    assert result is not None
    assert result.resolve() == main.resolve()


def test_linked_worktree_resolves_to_main(tmp_path: Path) -> None:
    """From a linked worktree, the first list entry (the main worktree) is
    returned - it has a `.git` dir."""
    from fno.paths import resolve_canonical_worktree

    main = tmp_path / "main"
    _init_repo(main)
    linked = tmp_path / "linked"
    _git("worktree", "add", "-q", str(linked), "-b", "feat", cwd=main)

    result = resolve_canonical_worktree(linked)
    assert result is not None
    assert result.resolve() == main.resolve()


def test_bare_repo_with_linked_worktree_skips_bare(tmp_path: Path) -> None:
    """A `git clone --bare` repo is listed first with a `bare` marker; the
    helper skips it and returns the real linked worktree, never the bare dir."""
    from fno.paths import resolve_canonical_worktree

    bare = tmp_path / "repo.git"
    bare.mkdir()
    _git("init", "-q", "--bare", str(bare), cwd=tmp_path)
    linked = tmp_path / "checkout"
    _git("worktree", "add", "-q", str(linked), "-b", "feat", cwd=bare)

    result = resolve_canonical_worktree(linked)
    assert result is not None
    assert result.resolve() == linked.resolve()
    assert result.resolve() != bare.resolve()


def test_bare_repo_without_worktree_returns_none(tmp_path: Path) -> None:
    """A bare repo with no checkout has no working tree -> None (caller falls
    back). The bare git dir is never returned."""
    from fno.paths import resolve_canonical_worktree

    bare = tmp_path / "repo.git"
    bare.mkdir()
    _git("init", "-q", "--bare", str(bare), cwd=tmp_path)

    assert resolve_canonical_worktree(bare) is None


def test_separate_git_dir_does_not_return_gitdir(tmp_path: Path) -> None:
    """`git init --separate-git-dir` reports the external git dir as the first
    `worktree` path; it has no `.git` child, so the helper skips it and returns
    None (caller falls back to --show-toplevel) rather than the gitdir."""
    from fno.paths import resolve_canonical_worktree

    wtree = tmp_path / "wtree"
    wtree.mkdir()
    ext_gitdir = tmp_path / "external-gitdir"
    _git("init", "-q", "--separate-git-dir", str(ext_gitdir), str(wtree), cwd=tmp_path)
    _git("commit", "--allow-empty", "-qm", "init", cwd=wtree)

    result = resolve_canonical_worktree(wtree)
    # First non-bare record is the gitdir mis-report -> None (caller falls back).
    assert result is None


def test_separate_git_dir_with_linked_does_not_return_sibling(tmp_path: Path) -> None:
    """separate-git-dir + a linked worktree: the gitdir is listed FIRST. The
    helper must NOT skip past it to the linked sibling (that would root config
    under an arbitrary worktree); the first non-bare record is a gitdir, so it
    returns None and the caller falls back to --show-toplevel (codex P2 #406)."""
    from fno.paths import resolve_canonical_worktree

    wtree = tmp_path / "wtree"
    wtree.mkdir()
    ext_gitdir = tmp_path / "external-gitdir"
    _git("init", "-q", "--separate-git-dir", str(ext_gitdir), str(wtree), cwd=tmp_path)
    _git("commit", "--allow-empty", "-qm", "init", cwd=wtree)
    linked = tmp_path / "linked"
    _git("worktree", "add", "-q", str(linked), "-b", "feat", cwd=wtree)

    result = resolve_canonical_worktree(wtree)
    assert result is None  # not `linked`, not the gitdir
    if result is not None:  # defensive: never the sibling or the gitdir
        assert result.resolve() not in (linked.resolve(), ext_gitdir.resolve())


def test_not_a_git_repo_returns_none(tmp_path: Path) -> None:
    from fno.paths import resolve_canonical_worktree

    plain = tmp_path / "plain"
    plain.mkdir()
    assert resolve_canonical_worktree(plain) is None
