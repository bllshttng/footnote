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


def propagate_returncode(returncode: int) -> int:
    """Normalise a ``subprocess.CompletedProcess.returncode`` for ``sys.exit``.

    Negative values denote signal-killed children; convert to ``128+|N|``
    so the shell-visible exit code matches the documented convention
    (SIGKILL -> 137, SIGTERM -> 143).
    """
    if returncode < 0:
        return 128 + (-returncode)
    return returncode
