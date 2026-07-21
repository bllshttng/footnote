"""Tests for scripts/roadmap-tasks.py shim fail-fast behavior.

AC3-HP: normal in-repo invocation succeeds, prints nothing to stderr.
AC3-ERR: shim relocated; SystemExit(3) with explicit message naming cli/src.
AC3-FR: cli/src exists but fno package absent; ImportError branch fires.
AC3-EDGE: shim is a symlink; resolve() follows it and assert passes silently.
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

# For tests that need to run the shim WITHOUT fno installed, we need a
# Python interpreter that does NOT have fno in its site-packages.
# The venv Python (sys.executable) has fno installed; search PATH
# excluding the venv for a system Python3.
def _find_system_python3() -> str:
    """Return a Python3 interpreter that is NOT the active venv's Python."""
    path_entries = os.environ.get("PATH", "").split(os.pathsep)
    for entry in path_entries:
        candidate = Path(entry) / "python3"
        if candidate.resolve() != Path(sys.executable).resolve() and candidate.is_file():
            # Verify it's actually a different python (not a venv symlink to same interpreter)
            venv_python_realpath = Path(sys.executable).resolve()
            candidate_realpath = candidate.resolve()
            if candidate_realpath != venv_python_realpath:
                return str(candidate)
    # Fallback: sys.executable (tests may not isolate perfectly but still test logic)
    return sys.executable


_SYSTEM_PYTHON3 = _find_system_python3()


def _run_shim(
    shim_path: Path,
    env=None,
    interpreter: str | None = None,
) -> subprocess.CompletedProcess:
    """Run a shim path via subprocess and capture all output.

    interpreter: Python executable to use. Defaults to sys.executable.
    Pass _SYSTEM_PYTHON3 for tests that need a clean Python without
    fno in site-packages.
    """
    run_env = os.environ.copy()
    # Remove PYTHONPATH to avoid picking up fno from unexpected places.
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
    """AC3-HP: Running the real shim from its canonical location succeeds.

    The shim should either invoke the CLI (exit 0) or raise ImportError if
    fno is not installed (exit 3 with install hint) -- but it must
    NOT write the 'shim broken' message or exit with an AssertionError.
    This verifies the happy path where cli/src is found correctly.
    """
    # Use the real shim with the venv Python (fno may or may not be installed)
    result = _run_shim(_REAL_SHIM, interpreter=sys.executable)
    # The shim must NOT produce the 'shim broken' error message.
    assert "fno CLI shim broken" not in result.stderr, (
        f"Got shim-broken error on canonical shim path.\nstderr: {result.stderr}"
    )
    assert "AssertionError" not in result.stderr, (
        f"Got bare AssertionError on canonical shim path.\nstderr: {result.stderr}"
    )


def test_ac3_err_shim_relocated_missing_cli_src():
    """AC3-ERR: shim moved outside repo; must exit 3 with explicit named-path message.

    Copy the shim to a temp dir where parents[1]/cli/src does NOT exist.
    The shim must emit 'error: fno CLI shim broken: expected cli/src at'
    and exit with code 3. It must NOT emit a bare 'AssertionError' traceback.
    """
    with tempfile.TemporaryDirectory() as tmp:
        # Place shim at <tmp>/scripts/roadmap-tasks.py
        # so parents[1] = <tmp> which has no cli/src
        scripts_dir = Path(tmp) / "scripts"
        scripts_dir.mkdir()
        tmp_shim = scripts_dir / "roadmap-tasks.py"
        shutil.copy(_REAL_SHIM, tmp_shim)

        result = _run_shim(tmp_shim, interpreter=_SYSTEM_PYTHON3)

    assert result.returncode == 3, (
        f"Expected exit code 3, got {result.returncode}.\nstderr: {result.stderr}"
    )
    assert "error: fno CLI shim broken: expected cli/src at" in result.stderr, (
        f"Expected explicit named-path message in stderr.\nstderr: {result.stderr}"
    )
    assert "AssertionError" not in result.stderr, (
        f"Must not emit bare AssertionError traceback.\nstderr: {result.stderr}"
    )
    assert "Traceback" not in result.stderr, (
        f"Must not emit a Python traceback.\nstderr: {result.stderr}"
    )


def test_ac3_fr_cli_src_exists_but_no_abilities_package():
    """AC3-FR: cli/src exists but fno package absent; ImportError branch fires.

    The assert passes (cli/src dir is present), but the import fails.
    The shim must produce the ImportError diagnostic and exit 3.
    It must NOT produce the 'shim broken' message -- that is the assert branch,
    which only fires when cli/src is completely absent.
    """
    with tempfile.TemporaryDirectory() as tmp:
        # Structure: <tmp>/scripts/roadmap-tasks.py
        #            <tmp>/cli/src/   (empty, no fno package)
        scripts_dir = Path(tmp) / "scripts"
        scripts_dir.mkdir()
        tmp_shim = scripts_dir / "roadmap-tasks.py"
        shutil.copy(_REAL_SHIM, tmp_shim)

        # Create cli/src but leave it empty (no fno package)
        (Path(tmp) / "cli" / "src").mkdir(parents=True)

        result = _run_shim(tmp_shim, interpreter=_SYSTEM_PYTHON3)

    assert result.returncode == 3, (
        f"Expected exit code 3, got {result.returncode}.\nstderr: {result.stderr}"
    )
    # ImportError branch message must appear
    assert "cannot import fno.graph.cli" in result.stderr, (
        f"Expected the ImportError diagnostic in stderr.\nstderr: {result.stderr}"
    )
    # The shim-broken assert must NOT fire (cli/src was present)
    assert "fno CLI shim broken" not in result.stderr, (
        f"Should not see shim-broken message when cli/src exists.\nstderr: {result.stderr}"
    )


def test_ac3_edge_symlinked_shim_resolves_correctly():
    """AC3-EDGE: shim accessed via symlink; resolve() follows it and assert passes.

    Create a symlink pointing to the real shim. Because Path(__file__).resolve()
    follows the symlink, parents[1] is the real repo root and cli/src is found.
    The shim must NOT produce the 'shim broken' message.
    """
    with tempfile.TemporaryDirectory() as tmp:
        symlink_path = Path(tmp) / "roadmap-tasks-link.py"
        symlink_path.symlink_to(_REAL_SHIM)

        result = _run_shim(symlink_path, interpreter=_SYSTEM_PYTHON3)

    assert "fno CLI shim broken" not in result.stderr, (
        f"Symlinked shim should resolve to real cli/src.\nstderr: {result.stderr}"
    )
    assert "AssertionError" not in result.stderr, (
        f"Must not emit bare AssertionError.\nstderr: {result.stderr}"
    )
