#!/usr/bin/env python3
"""Compatibility shim. Real implementation lives in ``fno backlog triage``.

Preference order:
    1. ``fno backlog triage <verb>`` when the installed CLI is on PATH
    2. The in-repo ``fno.graph.triage`` module (cli/src fallback)

Falls through with a loud error only when neither is available, which
should only happen on a broken install.

Kept in-repo so external callers (the ``/triage`` skill, hooks, users
with muscle memory for ``scripts/triage.py``) keep working across the
v1 -> v2 graph migration.
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


_repo_root = Path(__file__).resolve().parents[1]
_cli_src = _repo_root / "cli" / "src"
if _cli_src.is_dir() and str(_cli_src) not in sys.path:
    sys.path.insert(0, str(_cli_src))


def _abi_on_path() -> bool:
    for entry in os.environ.get("PATH", "").split(os.pathsep):
        if not entry:
            continue
        candidate = Path(entry) / "fno"
        try:
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return True
        except OSError:
            continue
    return False


def _forward(argv: list[str]) -> int:
    """Try installed CLI first; fall back to in-repo import; error otherwise."""
    if _abi_on_path():
        return subprocess.call(["fno", "backlog", "triage", *argv])

    try:
        from fno.graph.triage import cli as triage_app  # type: ignore[import-not-found]
    except ImportError:
        sys.stderr.write(
            f"error: fno CLI not found. Install with: uv tool install '{_repo_root / 'cli'}'\n"
            "       (or run from a repo where cli/src is on PYTHONPATH)\n"
        )
        return 3

    try:
        triage_app(argv, standalone_mode=True)
        return 0
    except SystemExit as exc:
        code = exc.code
        if isinstance(code, int):
            return code
        return 0 if code is None else 1


if __name__ == "__main__":
    sys.exit(_forward(sys.argv[1:]))
