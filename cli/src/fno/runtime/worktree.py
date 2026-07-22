"""Worktree management: create, list, remove git worktrees with .fno symlink.

Path convention: ``~/.fno/worktrees/{project_id}-{name}/`` (Plan
ab-3180b3f4). The legacy ``.claude/worktrees/{name}/`` shape is still
detected by ``list`` and ``remove`` so existing in-flight worktrees on
operator machines keep working through the transition (Option A from
the plan: leave old worktrees alone; only new ones use the new path).
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.worktree_paths import (
    _validate_component,
    legacy_worktree_path,
    worktree_base,
    worktree_path,
)


def _legacy_base_dir(repo_root: Path) -> Path:
    """The old ``<repo_root>/.claude/worktrees/`` location."""
    return repo_root / ".claude" / "worktrees"


def create_worktree(
    *,
    name: str,
    base: str = "main",
    repo_root: Path | None = None,
    project_id: str | None = None,
) -> dict:
    """Create a git worktree at ``~/.fno/worktrees/{proj}-{name}/``.

    Args:
        name: Worktree name (appended to feature/ for branch name).
        base: Base ref to branch from (default: main).
        repo_root: Root of the git repository. Defaults to cwd.
        project_id: Stable short project identifier. Resolved from
            ``.fno/settings.yaml`` (or git remote basename) when
            omitted. See :func:`fno.worktree_paths.resolve_project_id`.

    Returns:
        {"status": "created"|"already-exists", "worktree_path": str, "branch": str}

    Raises:
        RuntimeError: If git worktree add fails for any reason other than collision.
    """
    if repo_root is None:
        repo_root = Path.cwd()

    wt_path = worktree_path(name, project_id=project_id, repo_root=repo_root)
    branch = f"feature/{name}"

    # AC7 back-compat: detect a worktree already living at the legacy
    # ``.claude/worktrees/{name}/`` location and short-circuit rather
    # than create a duplicate at the new path.
    if wt_path.exists():
        existing = wt_path
    elif legacy_worktree_path(name, repo_root=repo_root).exists():
        existing = legacy_worktree_path(name, repo_root=repo_root)
    else:
        existing = None

    if existing is not None:
        return {
            "status": "already-exists",
            "worktree_path": str(existing),
            "branch": branch,
        }

    wt_path.parent.mkdir(parents=True, exist_ok=True)

    result = subprocess.run(
        ["git", "worktree", "add", "-b", branch, str(wt_path), base],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"git worktree add failed (exit {result.returncode}): {result.stderr.strip()}"
        )

    # Symlink .fno into the worktree so state files remain shared
    fno_src = (repo_root / ".fno").resolve()
    fno_link = wt_path / ".fno"
    if fno_src.exists() and not fno_link.exists():
        try:
            fno_link.symlink_to(fno_src)
        except OSError:
            # Filesystem may not support symlinks - document and continue
            pass

    return {
        "status": "created",
        "worktree_path": str(wt_path),
        "branch": branch,
    }


def list_worktrees(*, repo_root: Path | None = None) -> list[dict]:
    """List worktrees by querying git.

    Returns worktrees whose path is under EITHER the canonical
    ``~/.fno/worktrees/`` base OR the legacy
    ``<repo_root>/.claude/worktrees/`` base (transition window).

    Returns a list of ``{"name": str, "worktree_path": str, "branch": str}`` dicts.
    """
    # Resolve to absolute so the prefix filter (Path.relative_to) compares
    # apples-to-apples against the absolute paths emitted by
    # `git worktree list --porcelain` (Gemini MEDIUM PR #234).
    repo_root = Path(repo_root or Path.cwd()).resolve()

    result = subprocess.run(
        ["git", "worktree", "list", "--porcelain"],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"git worktree list failed: {result.stderr.strip()}")

    bases = (worktree_base(), _legacy_base_dir(repo_root))

    def under_any_base(path_str: str) -> bool:
        if not path_str:
            return False
        candidate = Path(path_str)
        for base in bases:
            try:
                candidate.relative_to(base)
                return True
            except ValueError:
                continue
        return False

    worktrees: list[dict] = []
    current: dict = {}
    for line in result.stdout.splitlines():
        if line.startswith("worktree "):
            if current and under_any_base(current.get("worktree_path", "")):
                worktrees.append(current)
            current = {"worktree_path": line[len("worktree "):]}
        elif line.startswith("branch "):
            branch = line[len("branch "):]
            if branch.startswith("refs/heads/"):
                branch = branch[len("refs/heads/"):]
            current["branch"] = branch
            current["name"] = Path(current["worktree_path"]).name
    if current and under_any_base(current.get("worktree_path", "")):
        worktrees.append(current)

    return worktrees


def remove_worktree(
    *,
    name: str,
    repo_root: Path | None = None,
    prune_branch: bool = False,
    project_id: str | None = None,
) -> dict:
    """Remove a worktree created by ``create_worktree``.

    Resolution order for the path to remove (Gemini HIGH PR #234):

    1. If ``name`` is a full canonical directory name (e.g.
       ``fno-foo`` from ``list_worktrees``), use
       ``~/.fno/worktrees/{name}/`` directly. This is the round-
       trip case: list emits ``fno-foo`` then remove takes the same
       string back.
    2. Else if a canonical worktree exists at
       ``~/.fno/worktrees/{proj}-{name}/`` (the slug-only case
       where the caller passes ``foo`` and we re-prepend ``proj-``),
       use that.
    3. Else fall back to the legacy ``<repo>/.claude/worktrees/{name}/``
       (AC7 back-compat).

    The earlier "canonical-only-if-exists, else legacy" shape silently
    double-prepended the project id when called as
    ``remove_worktree(name="fno-foo")`` and missed every canonical
    worktree listed by ``list_worktrees`` until the path drifted.

    Args:
        name: Worktree name OR full directory name (both work).
        repo_root: Root of the git repository. Defaults to cwd.
        prune_branch: If True, also delete the feature/{name} branch.
        project_id: Stable project identifier; resolved from settings
            when omitted.

    Returns:
        {"status": "removed", "worktree_path": str}

    Raises:
        RuntimeError: If git worktree remove fails.
    """
    if repo_root is None:
        repo_root = Path.cwd()

    branch = f"feature/{name}"

    # Validate name explicitly before constructing the canonical-full
    # path so a name like "../escape" can't escape the worktree base.
    # worktree_path() validates internally; canonical_full bypasses that
    # so we re-run the same check here for defense-in-depth.
    _validate_component(name, kind="name")
    canonical_full = worktree_base() / name
    canonical_slug = worktree_path(name, project_id=project_id, repo_root=repo_root)
    legacy = legacy_worktree_path(name, repo_root=repo_root)

    if canonical_full.exists():
        wt_path = canonical_full
    elif canonical_slug.exists():
        wt_path = canonical_slug
    else:
        wt_path = legacy

    result = subprocess.run(
        ["git", "worktree", "remove", "--force", str(wt_path)],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"git worktree remove failed (exit {result.returncode}): {result.stderr.strip()}"
        )

    branch_pruned = False
    branch_prune_error: str | None = None
    if prune_branch:
        prune_result = subprocess.run(
            ["git", "branch", "-D", branch],
            cwd=str(repo_root),
            capture_output=True,
            text=True,
            check=False,
        )
        if prune_result.returncode == 0:
            branch_pruned = True
        else:
            branch_prune_error = (prune_result.stderr or "").strip() or (
                f"git branch -D exited {prune_result.returncode}"
            )

    result_dict: dict = {"status": "removed", "worktree_path": str(wt_path)}
    if prune_branch:
        result_dict["branch_pruned"] = branch_pruned
        if branch_prune_error:
            result_dict["branch_prune_error"] = branch_prune_error
    return result_dict
