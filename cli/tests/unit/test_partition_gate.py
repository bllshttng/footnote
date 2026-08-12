"""Tests for scripts/ci/check-tracker-partition.sh.

The gate enforces zero overlap (except the id key) between the footnote-owned
sidecar and the tracker read interface. Its --self-test is the positive
control: it proves field extraction and overlap detection both fire on
synthetic input, so a green real-check cannot be the "instrument never ran"
absence. These tests run both modes.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
GATE = REPO / "scripts" / "ci" / "check-tracker-partition.sh"


def test_self_test_fires():
    # Positive control: the gate must report its self-test passing. An exit 0
    # here without the marker would mean the controls are vacuous.
    r = subprocess.run(
        ["bash", str(GATE), "--self-test"], capture_output=True, text=True
    )
    assert r.returncode == 0, r.stderr
    assert "self-test OK" in r.stdout


def test_partition_holds_on_real_models():
    r = subprocess.run(
        ["bash", str(GATE)], capture_output=True, text=True, cwd=str(REPO)
    )
    assert r.returncode == 0, r.stderr
    assert "overlap is key-only" in r.stdout
