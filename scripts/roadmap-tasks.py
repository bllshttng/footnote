#!/usr/bin/env python3
"""Compatibility shim. Real implementation lives in the backlog graph CLI.

Everything forwards through the ``fno`` front door's backlog compat
spelling, which rides the same graph surface the retired direct import
used; the in-repo ``fno.graph.cli`` import leg is gone with the consumers
repoint (x-9323).
"""
import subprocess
import sys


def _forward(argv: list[str]) -> int:
    """One front-door call; a missing `fno` refuses loudly."""
    try:
        return subprocess.call(["fno", "backlog", *argv])
    except FileNotFoundError:
        sys.stderr.write(
            "error: fno CLI not found. Install fno, then re-run: "
            "fno backlog " + " ".join(argv) + "\n"
        )
        return 3


if __name__ == "__main__":
    sys.exit(_forward(sys.argv[1:]))
