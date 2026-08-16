"""Tests for scripts/ci/check-autonomy-registry.sh (x-aaaf wave 3.3, AC6-CON).

The negative control the plan requires: a fake unregistered spawner must turn
CI red, naming it, and removing it must turn CI green again. Also asserts the
real repo state passes today, so the baseline cannot silently drift stale.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci" / "check-autonomy-registry.sh"


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
