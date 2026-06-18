"""Subprocess helpers shared by the in-package ``fno pr`` ports.

The ``fno pr {merge,verify,rebase}`` verbs were ported from bash to in-package
Python that shells to ``gh`` / ``git`` (ab-d4c98550). This module centralises
the one idiom they all need: run an external tool, capture text output, and
distinguish "tool not installed" from "tool ran and failed". Centralising it
keeps the ``gh``-version-drift fixes in one place (Domain Pitfall: pin gh
fields, parse JSON in one spot).
"""
from __future__ import annotations

import subprocess
from dataclasses import dataclass
from typing import Mapping, Optional, Sequence


class ToolMissing(Exception):
    """Raised when an external binary (``gh`` / ``git``) is not on PATH.

    The bash scripts mapped a missing ``gh`` to a specific exit code (127 for
    merge) rather than a traceback; callers catch this to preserve that
    contract instead of leaking a ``FileNotFoundError``.
    """

    def __init__(self, tool: str) -> None:
        self.tool = tool
        super().__init__(f"{tool} not found on PATH")


@dataclass
class Result:
    """The captured outcome of an external command (text mode)."""

    returncode: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.returncode == 0


def run(
    cmd: Sequence[str],
    *,
    cwd: Optional[str] = None,
    env: Optional[Mapping[str, str]] = None,
    input_text: Optional[str] = None,
    timeout: Optional[float] = None,
) -> Result:
    """Run ``cmd`` capturing stdout/stderr as text.

    Raises :class:`ToolMissing` when the binary itself is absent (the bash
    ``command -v`` guard), so callers can preserve the script's missing-tool
    exit code rather than surfacing a Python traceback.
    """
    try:
        proc = subprocess.run(
            list(cmd),
            cwd=cwd,
            env=dict(env) if env is not None else None,
            input=input_text,
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
    except FileNotFoundError as exc:
        # FileNotFoundError fires when argv[0] is not on PATH. (A missing cwd
        # also raises it, but callers pass an existing cwd.)
        raise ToolMissing(cmd[0]) from exc
    return Result(returncode=proc.returncode, stdout=proc.stdout or "", stderr=proc.stderr or "")
