from __future__ import annotations

import subprocess
from pathlib import Path

import pytest


def _find_rust_bin() -> Path | None:
    start = Path(__file__).resolve().parent
    for parent in [start, *start.parents]:
        crate = parent / "crates" / "fno-agents"
        if crate.is_dir():
            for profile in ("release", "debug"):
                cand = crate / "target" / profile / "fno-agents"
                if cand.is_file():
                    return cand
            return None
    return None


RUST_BIN = _find_rust_bin()
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
