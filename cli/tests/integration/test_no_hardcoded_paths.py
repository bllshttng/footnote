"""Pytest wrapper for scripts/ci/check-no-hardcoded-paths.sh.

Task 3.8 of plan 2026-05-14-path-config-impl.

Ensures the CI hardcoded-path grep gate passes in the working tree.
Runs the shell script and asserts exit 0.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path


# Resolve the repo root via FNO_REPO_ROOT (test isolation) or git.
def _repo_root() -> Path:
    env_root = os.environ.get("FNO_REPO_ROOT")
    if env_root:
        return Path(env_root).resolve()
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode == 0 and result.stdout.strip():
        return Path(result.stdout.strip()).resolve()
    return Path(__file__).resolve().parents[3]


def test_ac1_hp_no_hardcoded_paths_in_tree() -> None:
    """AC1-HP: check-no-hardcoded-paths.sh exits 0 on the current working tree."""
    repo = _repo_root()
    gate = repo / "scripts" / "ci" / "check-no-hardcoded-paths.sh"
    result = subprocess.run(
        ["bash", str(gate)],
        capture_output=True,
        text=True,
        cwd=str(repo),
    )
    assert result.returncode == 0, (
        f"check-no-hardcoded-paths.sh exited {result.returncode}.\n"
        f"STDOUT:\n{result.stdout}\n"
        f"STDERR:\n{result.stderr}"
    )
