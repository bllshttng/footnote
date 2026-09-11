"""Stage a filtered copy of this checkout for harness plugin installs.

The harness plugin caches copy the REPO ROOT wholesale (Claude's directory
marketplace copies the source dir; the Codex dev channel stages the same
tree). Left alone that copy carries `crates/*/target` and
`.claude/worktrees`: measured 19 GB in the Claude cache and 22 GB in the
Codex cache, most of it regenerable build output (x-7ca7). The stage is the
one thing installs copy: tracked plus untracked-but-not-ignored files only,
rebuilt on every install.
"""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

__all__ = ["build_stage", "stage_root"]


def stage_root() -> Path:
    """`<state_dir>/plugin-stage/fno` - the whole stage is replaced per build."""
    from fno.paths import state_dir

    return state_dir() / "plugin-stage" / "fno"


def _stage_file_list(root: Path) -> list[str]:
    """Tracked + untracked-but-not-ignored paths, NUL-delimited so any
    filename survives. `--exclude-standard` is the whole filter: gitignore is
    what keeps target/, worktrees and venvs out of the stage."""
    out = subprocess.run(
        ["git", "ls-files", "-z", "-co", "--exclude-standard"],
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout
    return [p for p in out.decode("utf-8", "replace").split("\0") if p]


def build_stage(root: Path | None = None) -> Path:
    """(Re)build the stage from `root` (default: this repo's canonical root).

    Idempotent: the previous stage tree is removed wholesale first, so files
    deleted upstream cannot survive as stale stage entries.
    """
    from fno.paths import resolve_canonical_repo_root

    root = Path(root) if root is not None else resolve_canonical_repo_root()
    dest = stage_root()
    shutil.rmtree(dest.parent, ignore_errors=True)
    dest.parent.mkdir(parents=True, exist_ok=True)
    for rel in _stage_file_list(root):
        src = root / rel
        if not src.is_file():
            continue  # cached by git but deleted in the working tree
        target = dest / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, target)
    return dest
