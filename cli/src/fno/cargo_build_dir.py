"""Reclaim a worktree's cargo build hash dir at removal time.

Cargo writes intermediates OUTSIDE the checkout (``<base>/{workspace-path-hash}``
under ``build.build-dir``), so removing a worktree orphans its hash dir until a
later sweep reaches it. Resolution reads the workspace manifest, so the removal
must run BEFORE the checkout is deleted. Best-effort by contract: an unreadable
resolution leaves the dir to the sweep, never fails the removal.

The shell callers (hooks, archive, the sweeps, the Rust reaper) share
``scripts/lib/cargo-build-dir.sh``; this package copy exists so the Python
verbs need no repo-root script at runtime (the shellout-drift guard, US4).
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

from fno.paths import cargo_build_dir_value

_HASH_SUFFIX = "/{workspace-path-hash}"


def build_dir_base() -> Path:
    """The managed build base: the env override, else the config base."""
    override = os.environ.get("FNO_CARGO_TARGETS_BASE")
    if override:
        return Path(os.path.expanduser(override))
    base = cargo_build_dir_value()
    if base.endswith(_HASH_SUFFIX):
        base = base[: -len(_HASH_SUFFIX)]
    return Path(base)


def remove_build_dir_for_worktree(worktree: Path) -> bool:
    """Remove the build hash dir cargo resolves for ``worktree``.

    Deletes only under the managed build base and only behind cargo's own
    ``CACHEDIR.TAG`` - the same two conjuncts the sweep deletes under.
    Returns True when at least one hash dir was removed.
    """
    try:
        base = build_dir_base().resolve(strict=True)
    except OSError:
        return False
    removed = False
    for manifest in sorted(worktree.glob("crates/*/Cargo.toml")):
        try:
            raw = subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--no-deps",
                 "--manifest-path", str(manifest)],
                capture_output=True, text=True, check=True, timeout=60,
            ).stdout
            resolved = Path(json.loads(raw)["build_directory"]).resolve(strict=True)
        except (OSError, subprocess.SubprocessError, ValueError, KeyError):
            continue
        if not (resolved / "CACHEDIR.TAG").is_file():
            continue
        if base not in resolved.parents:
            continue
        shutil.rmtree(resolved, ignore_errors=True)
        removed = removed or not resolved.exists()
    return removed
