"""The shared native door: tests that drive this checkout's fno-agents
binary resolve it through the same finder and skip the same way."""
from __future__ import annotations

import os
import subprocess

import pytest


def run_native(*args: str) -> tuple[int, str, str]:
    """Run `fno-agents <args>`; skip when this checkout has no dev build."""
    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    proc = subprocess.run(
        [str(binary), *args],
        capture_output=True,
        text=True,
        env={**os.environ, "FNO_TRACKER_BACKEND": "graph"},
    )
    return proc.returncode, proc.stdout, proc.stderr
