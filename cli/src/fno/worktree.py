"""Worktree helpers with live callers.

The walker's worktree-manager class, its ``<prefix>/<slug>-<node>`` mint,
and the runtime/adapters worktree minters are gone: branch naming
is the one Rust resolver (``crates/fno-agents/src/node_branch.rs``),
reached through ``fno backlog get <id> --field _branch``, and ``worktree
ensure`` (``fno.worktree_cli.cli``) owns the live create/reuse path. What
survives here: the setup hook target_cli runs after creating a worktree,
and the config reader graph/maintain.py uses to resolve the worktree base.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Optional

from fno._subprocess_util import run_bounded

# 120s bounds the hook well below the 10+ minute cleanup-leg stalls on record.
_SETUP_HOOK_TIMEOUT_S = 120


def _read_worktrees_base_from(settings_path: Path) -> Optional[str]:
    """Return paths.worktrees_base from one flat config.toml (or legacy
    settings.yaml), or None. Every intermediate key is isinstance-checked so a
    malformed file returns None instead of raising.
    """
    if not settings_path.exists():
        return None
    from fno.config import read_config_flat

    paths = read_config_flat(settings_path).get("paths")
    base = paths.get("worktrees_base") if isinstance(paths, dict) else None
    return base if isinstance(base, str) and base else None


def _run_setup_worktree_hook(
    repo_root: Path,
    worktree_path: Path,
    timeout: float = _SETUP_HOOK_TIMEOUT_S,
) -> tuple[int, str]:
    """Best-effort: run scripts/setup/setup-worktree.sh inside the new worktree.

    The script symlinks gitignored shared state (.fno/, internal/,
    .claude/ subdirs) from the canonical project into the worktree. Without
    this step, dispatches into worktrees have no link to the canonical
    .fno/ state, so target gates can't see backlog mutations from sibling
    worktrees, codemap goes stale, and inbox drain breaks.

    Returns (returncode, stderr_tail). -1 means the script was not found
    (silently tolerated); 124 means it exceeded ``timeout`` and its process
    group was killed. Any other non-zero is logged but never raised.
    """
    script = repo_root / "scripts" / "setup" / "setup-worktree.sh"
    if not script.exists():
        return (-1, "")
    try:
        proc = run_bounded(
            ["bash", str(script)],
            timeout=timeout,
            capture_output=True,
            text=True,
            cwd=str(worktree_path),
            env={**os.environ, "CANONICAL": str(repo_root), "WORKTREE": str(worktree_path)},
        )
    except subprocess.TimeoutExpired:
        return (124, f"setup-worktree.sh exceeded {timeout:.0f}s; its process group was killed")
    tail = (proc.stderr or proc.stdout or "")[-500:]
    return (proc.returncode, tail)
