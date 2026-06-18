"""Shared helpers used by RuntimeAdapter implementations.

Per Locked Decision 5 of the cross-CLI design doc, primitives that don't
depend on the underlying CLI (e.g., git worktree creation) live here.
The Protocol's three primitives (spawn_worker, call_api, health) stay
per-adapter because they DO depend on the CLI.

Worktree path: ``~/.fno/worktrees/{project_id}-{name}/`` (Plan
ab-3180b3f4). The legacy ``.claude/worktrees/{name}/`` location is
detected only for branch-reuse purposes (AC7) - new worktrees always
land at the canonical location.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.worktree_paths import legacy_worktree_path, worktree_path


def create_worktree(
    *,
    name: str,
    base: str = "main",
    project_id: str | None = None,
    repo_root: Path | None = None,
) -> dict:
    """Create a git worktree at ``~/.fno/worktrees/{proj}-{name}/``.

    CLI-agnostic: uses only ``git worktree`` and ``git``. All adapters
    delegate here so the implementation lives in one place. The status
    vocabulary (``"created" | "already-exists"``) matches
    :func:`fno.runtime.worktree.create_worktree` so callers can
    switch layers without translating return shapes.

    ``project_id`` is resolved from ``.fno/settings.yaml`` (or
    derived from ``git remote get-url origin``) when omitted; see
    :func:`fno.worktree_paths.resolve_project_id` for the chain.

    ``repo_root`` is the directory in which the legacy
    ``.claude/worktrees/{name}/`` probe runs and against which the
    settings/remote-derivation chain resolves the project id. Defaults
    to ``Path.cwd()`` so the existing in-session callers (which run
    with cwd == repo root) keep working without an API change. Pass
    it explicitly when invoking from outside the repo root - silently
    resolving against an unrelated cwd would pick the wrong
    project_id (code-reviewer finding #1).

    Returns:
        ``{"worktree_path": str, "branch": str, "status": "created" | "already-exists"}``

    Raises:
        RuntimeError: When ``git worktree add`` fails. The captured
        stderr is included in the message so callers see why the
        underlying git command refused (e.g., not in a git repo, locked
        worktree, branch collision).
    """
    wt_path = worktree_path(name, project_id=project_id, repo_root=repo_root)
    branch = f"feature/{name}"

    # AC7: a worktree may already exist either at the canonical new path
    # or at the legacy ``.claude/worktrees/{name}/`` location. Either
    # shape short-circuits with ``status: already-exists`` so the caller
    # can pick up where they left off without us trampling the directory.
    if wt_path.exists():
        return {
            "worktree_path": str(wt_path),
            "branch": branch,
            "status": "already-exists",
        }
    legacy_path = legacy_worktree_path(name, repo_root=repo_root)
    if legacy_path.exists():
        return {
            "worktree_path": str(legacy_path),
            "branch": branch,
            "status": "already-exists",
        }

    wt_path.parent.mkdir(parents=True, exist_ok=True)

    # A previous worktree may have been removed (`git worktree remove` or rm -rf
    # on the path) while leaving the underlying branch in place. In that case
    # the worktree path no longer exists (we passed the early-return above) but
    # `git worktree add -b <branch> <base>` would fail with "branch already
    # exists". Probe `git show-ref` to decide whether to create a fresh branch
    # or check out the existing one.
    branch_exists = subprocess.run(
        ["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"],
        check=False,
    ).returncode == 0

    if branch_exists:
        cmd = ["git", "worktree", "add", str(wt_path), branch]
    else:
        cmd = [
            "git", "worktree", "add",
            str(wt_path),
            "-b", branch,
            base,
        ]
    try:
        subprocess.run(cmd, check=True, capture_output=True, text=True)
    except subprocess.CalledProcessError as exc:
        stderr = (exc.stderr or "").strip()
        raise RuntimeError(
            f"git worktree add failed (exit {exc.returncode}): {stderr or '(no stderr)'}"
        ) from exc

    return {
        "worktree_path": str(wt_path),
        "branch": branch,
        "status": "created",
    }
