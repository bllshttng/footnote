"""Subprocess helpers shared by the fno wrappers.

The wrappers forward args to canonical bash scripts and propagate the
returncode unchanged. Python's ``subprocess.run().returncode`` returns
negative integers for signal-killed children (SIGKILL=-9, SIGTERM=-15)
while shell convention is ``128+N``. Passing a negative integer to
``typer.Exit(code=...)`` /  ``sys.exit`` ends up as a low-byte modulo on
POSIX, so callers branching on ``rc==1`` / ``rc==2`` see arbitrary
positive bytes instead of the expected signal-derived code.

``propagate_returncode`` normalises the value once at the boundary so
every wrapper produces the same shell-visible code for the same exit
condition. Past panel finding: ``feedback_python_subprocess_negative_returncode``.
"""
from __future__ import annotations

import shutil
import sys
from pathlib import Path


def fno_py_cmd() -> list[str]:
    """Resolve the `fno-py` console script (the Python CLI) as an argv prefix for
    Python self-shellouts, robust to PATH.

    The Rust mux binary owns `fno` and forwards to `fno-py` by ABSOLUTE path; a
    bare ``["fno-py", ...]`` subprocess instead relies on `fno-py` being on PATH,
    which fails on a cargo-only install where only ``~/.cargo/bin`` (the mux) is
    on PATH and ``~/.local/bin`` (fno-py) is not (codex peer finding). Resolve it
    without a PATH dependency: PATH first, then the console script beside the
    running interpreter (when this code runs AS fno-py, `sys.executable`'s sibling
    IS it), then the bare name so a genuinely-missing CLI surfaces a real
    subprocess error rather than a silent no-op.
    """
    found = shutil.which("fno-py")
    if found:
        return [found]
    sibling = Path(sys.executable).parent / "fno-py"
    if sibling.exists():
        return [str(sibling)]
    return ["fno-py"]


def propagate_returncode(returncode: int) -> int:
    """Normalise a ``subprocess.CompletedProcess.returncode`` for ``sys.exit``.

    Negative values denote signal-killed children; convert to ``128+|N|``
    so the shell-visible exit code matches the documented convention
    (SIGKILL -> 137, SIGTERM -> 143).
    """
    if returncode < 0:
        return 128 + (-returncode)
    return returncode
