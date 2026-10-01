"""Tests for the scripts/roadmap-tasks.py compatibility shim.

The shim forwards everything through the `fno` front door's backlog compat
spelling. Its one error path: `fno` missing from PATH refuses loudly with
rc=3 instead of dying in an import.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

SHIM_PATH = Path(__file__).resolve().parents[3] / "scripts" / "roadmap-tasks.py"


def test_shim_missing_front_door_refuses_loudly(tmp_path: Path) -> None:
    """No `fno` on PATH: the shim exits 3 naming the install remedy, and
    the message names the front-door spelling it tried to run."""
    fake_repo = tmp_path / "fake-repo"
    scripts_dir = fake_repo / "scripts"
    scripts_dir.mkdir(parents=True)
    fake_shim = scripts_dir / "roadmap-tasks.py"
    shutil.copy2(SHIM_PATH, fake_shim)

    env = dict(os.environ, PATH="/usr/bin:/bin")
    result = subprocess.run(
        [sys.executable, str(fake_shim), "get", "ab-12345678"],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )

    assert result.returncode == 3, (
        f"expected rc=3 with no front door, got rc={result.returncode}; "
        f"stderr={result.stderr!r}"
    )
    assert "fno CLI not found" in result.stderr, (
        f"stderr should carry the missing-front-door remedy, got: {result.stderr!r}"
    )
    assert "fno backlog" in result.stderr, (
        f"stderr should name the front-door spelling; got: {result.stderr!r}"
    )
