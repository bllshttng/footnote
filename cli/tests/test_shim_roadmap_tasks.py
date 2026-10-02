"""Tests for scripts/roadmap-tasks.py shim behavior.

The shim forwards everything through the `fno` front door, so the old
cli/src-layout branches are gone. What remains to pin:

AC3-HP: the canonical shim runs and never emits a traceback or the retired
shim-broken diagnostic.
AC3-ERR: the shim relocated outside any repo layout refuses identically
(no `fno` on PATH -> rc 3 with the install remedy), because the shim no
longer resolves anything from its own location.
"""
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# The real shim lives two levels up from cli/ (i.e. repo_root/scripts/)
_REPO_ROOT = Path(__file__).resolve().parents[2]
_REAL_SHIM = _REPO_ROOT / "scripts" / "roadmap-tasks.py"


def _run_shim(shim_path: Path, env=None, interpreter: str | None = None):
    """Run a shim path via subprocess and capture all output."""
    run_env = os.environ.copy()
    # Remove PYTHONPATH so the child resolves fno like a bare invocation.
    run_env.pop("PYTHONPATH", None)
    if env:
        run_env.update(env)
    exe = interpreter or sys.executable
    return subprocess.run(
        [exe, str(shim_path)],
        capture_output=True,
        text=True,
        env=run_env,
    )


def test_ac3_hp_normal_invocation_no_stderr():
    """AC3-HP: the real shim from its canonical location never emits the
    retired shim-broken diagnostic or a bare traceback."""
    result = _run_shim(_REAL_SHIM)
    assert "fno CLI shim broken" not in result.stderr, (
        f"Got shim-broken error on canonical shim path.\nstderr: {result.stderr}"
    )
    assert "AssertionError" not in result.stderr, (
        f"Got bare AssertionError on canonical shim path.\nstderr: {result.stderr}"
    )


def test_ac3_err_shim_relocated_refuses_without_front_door(tmp_path):
    """AC3-ERR: the shim copied outside any repo layout behaves exactly as
    in-repo when `fno` is absent: exit 3 with the install remedy naming the
    front-door spelling. Location independence is the point of forwarding."""
    scripts_dir = tmp_path / "scripts"
    scripts_dir.mkdir()
    tmp_shim = scripts_dir / "roadmap-tasks.py"
    shutil.copy(_REAL_SHIM, tmp_shim)

    stripped = dict(os.environ, PATH="/usr/bin:/bin")
    result = _run_shim(tmp_shim, env=stripped)

    assert result.returncode == 3, (
        f"Expected exit code 3 with no front door, got {result.returncode}.\n"
        f"stderr: {result.stderr}"
    )
    assert "fno CLI not found" in result.stderr, (
        f"Expected the install remedy in stderr.\nstderr: {result.stderr}"
    )
    assert "Traceback" not in result.stderr, (
        f"Must not emit a Python traceback.\nstderr: {result.stderr}"
    )
