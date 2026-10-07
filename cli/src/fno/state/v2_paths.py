"""v1 / v2 state path resolution (phase 04).

The v2 spine lives under ``.fno/v2/`` so it can coexist with the
in-flight v1 state machine. ``fno do state show --v2`` reads these paths.
"""

from __future__ import annotations

from pathlib import Path


def v2_root(repo_root: Path) -> Path:
    return repo_root / ".fno" / "v2"


def v2_state_path(repo_root: Path) -> Path:
    return v2_root(repo_root) / "target-state.md"


def v1_state_path(repo_root: Path) -> Path:
    return repo_root / ".fno" / "target-state.md"
