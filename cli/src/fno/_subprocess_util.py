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

import os
import shutil
import signal
import subprocess
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
    # sys.executable can be empty/None in embedded or frozen interpreters; guard
    # before Path() so resolution degrades to the bare name rather than raising.
    if sys.executable:
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


def run_bounded(
    cmd: list[str],
    *,
    timeout: float,
    capture_output: bool = False,
    text: bool = False,
    **popen_kwargs: object,
) -> subprocess.CompletedProcess:
    """Like ``subprocess.run(cmd, timeout=timeout)``, but kills the whole
    process group on timeout or interrupt, not just the direct child.

    ``subprocess.run(timeout=)`` only kills the child it spawned. A bash
    script's grandchildren (e.g. a nested ``fno ... --apply`` leg) survive
    it and can keep acting after the caller has already reported failure.
    Starting a new session (``start_new_session=True``) puts the whole tree
    in its own process group so ``os.killpg`` reaches all of it -- and
    because that group is detached from the terminal, Ctrl-C never reaches
    it either, so the ``BaseException`` arm below has to kill it too.
    """
    proc = subprocess.Popen(
        cmd,
        start_new_session=True,
        stdout=subprocess.PIPE if capture_output else None,
        stderr=subprocess.PIPE if capture_output else None,
        text=text,
        **popen_kwargs,
    )
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        _killpg_quiet(proc.pid)
        proc.communicate()
        raise
    except BaseException:
        _killpg_quiet(proc.pid)
        proc.communicate()
        raise
    return subprocess.CompletedProcess(cmd, proc.returncode, stdout, stderr)


def _killpg_quiet(pid: int) -> None:
    try:
        os.killpg(pid, signal.SIGKILL)
    except OSError:
        pass
