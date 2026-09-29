"""Worktree listing for the runtime surface.

The walker-era create/remove minters are gone (x-93c9); ``list_worktrees``
is the one survivor, feeding provenance/runtime_attempts.py. Path
convention: ``~/.fno/worktrees/{project_id}-{name}/``. The legacy
``.claude/worktrees/{name}/`` shape is still detected so existing
in-flight worktrees keep working.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.worktree_paths import worktree_base


def _legacy_base_dir(repo_root: Path) -> Path:
    """The old ``<repo_root>/.claude/worktrees/`` location."""
    return repo_root / ".claude" / "worktrees"


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
