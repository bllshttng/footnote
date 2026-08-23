from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.rust_binary import find_dev_binary


RUST_BIN = find_dev_binary()
requires_rust = pytest.mark.skipif(
    RUST_BIN is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


@requires_rust
def test_keyless_dispatch_to_terminal_reports_effect_sequence() -> None:
    # The script strips provider keys and FNO_* itself, so the wrapper passes
    # its own environment through; hermeticity must not depend on this file.
    repo_root = Path(__file__).parents[3]
    result = subprocess.run(
        ["python3", "scripts/evals/keyless_smoke.py"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        timeout=90,
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert "keyless smoke: PASS" in result.stdout
    assert "dispatch -> claims -> registry -> terminal -> receipts" in result.stdout
