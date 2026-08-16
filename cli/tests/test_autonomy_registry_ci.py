"""Tests for scripts/ci/check-autonomy-registry.sh (x-aaaf wave 3.3, AC6-CON).

The negative control the plan requires: a fake unregistered spawner must turn
CI red, naming it, and removing it must turn CI green again. Also asserts the
real repo state passes today, so the baseline cannot silently drift stale.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci" / "check-autonomy-registry.sh"

# Both tests scan the WHOLE cli/src tree while one of them mutates it (the
# probe file). Under `-n auto` xdist can split them across workers, and the
# baseline scan then sees the other test's transient probe and fails on
# interleaving, not on a real spawner (observed on the dirty lane, gw0/gw1).
# One group name pins them to a single worker, where file order serializes
# them for free.
pytestmark = pytest.mark.xdist_group("autonomy-registry")


def _run() -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(SCRIPT)], cwd=ROOT, capture_output=True, text=True,
    )


def test_current_repo_matches_the_baseline() -> None:
    """The baseline must describe today's tree, or the ratchet is already stale."""
    result = _run()
    assert result.returncode == 0, result.stdout + result.stderr


def test_negative_control_unregistered_spawner_fails_ci_then_clean_again() -> None:
    """AC6-CON: a new spawn-shaped call site not in the baseline fails CI and
    names it; removing it restores a clean pass."""
    probe = ROOT / "cli" / "src" / "fno" / "_ci_probe_fake_spawner.py"
    assert not probe.exists(), "stale probe file from a prior failed run"
    probe.write_text(
        "def _fake_autonomous_spawner():\n"
        "    cmd = [\n"
        '        "fno",\n'
        '        "agents",\n'
        '        "spawn",\n'
        '        "--name",\n'
        '        "fake",\n'
        "    ]\n",
        encoding="utf-8",
    )
    try:
        result = _run()
        assert result.returncode == 1, result.stdout + result.stderr
        assert "_ci_probe_fake_spawner.py::_fake_autonomous_spawner" in (
            result.stdout + result.stderr
        )
    finally:
        probe.unlink()

    clean = _run()
    assert clean.returncode == 0, clean.stdout + clean.stderr
