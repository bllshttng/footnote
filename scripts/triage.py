#!/usr/bin/env python3
"""Compatibility shim. Real implementation lives in ``fno backlog triage``.

Everything forwards through the ``fno`` front door; the retired in-repo
``fno.graph.triage`` import leg is gone with the consumers repoint.
Kept in-repo so external callers (the ``/triage`` skill, hooks, users
with muscle memory for ``scripts/triage.py``) keep working.
"""
from __future__ import annotations

import subprocess
import sys


def _forward(argv: list[str]) -> int:
    """One front-door call; a missing `fno` refuses loudly."""
    try:
        return subprocess.call(["fno", "backlog", "triage", *argv])
    except FileNotFoundError:
        sys.stderr.write(
            "error: fno CLI not found. Install fno, then re-run: "
            "fno backlog triage " + " ".join(argv) + "\n"
        )
        return 3


if __name__ == "__main__":
    sys.exit(_forward(sys.argv[1:]))
