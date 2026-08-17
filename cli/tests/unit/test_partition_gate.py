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
    # Positive assertion of the join key, not an absence: the gate must report
    # the overlap is exactly {id}, so a lockstep rename of `id` on both sides
    # (overlap becomes empty) would fail this, not pass vacuously.
    assert "exactly the join key ['id']" in r.stdout
    # Every tracker-owned projection is named in the receipt: the selection
    # projection joins the inspection set, so the marker proves the gate saw
    # more than TrackerNode.
    assert "TrackerCandidate" in r.stdout


def test_gate_fails_on_subclass_projection_overlap(tmp_path):
    # A forbidden mirror smuggled onto a SUBCLASS projection (not TrackerNode
    # itself) must still fail the gate: the union rule covers every projection.
    poisoned = tmp_path / "poison"
    types = poisoned / "cli/src/fno/tracker/types.py"
    sidecar = poisoned / "cli/src/fno/tracker/sidecar.py"
    types.parent.mkdir(parents=True)
    types.write_text(
        "class TrackerNode:\n"
        "    id: str\n"
        "    title: str = None\n"
        "class TrackerCandidate(TrackerNode):\n"
        "    batch: str = None\n"
    )
    sidecar.write_text("class Sidecar:\n    id: str\n    batch: str = None\n")
    r = subprocess.run(
        ["bash", str(GATE), str(poisoned)], capture_output=True, text=True
    )
    assert r.returncode == 1
    assert "batch" in r.stderr


def test_gate_fails_when_id_key_missing(tmp_path):
    # If `id` vanished from both models, an absence-based gate would pass. This
    # proves the gate fails instead (the join key is required on both sides).
    poisoned = tmp_path / "poison"
    types = poisoned / "cli/src/fno/tracker/types.py"
    sidecar = poisoned / "cli/src/fno/tracker/sidecar.py"
    types.parent.mkdir(parents=True)
    # Neither class declares `id`:
    types.write_text("class TrackerNode:\n    title: str = None\n")
    sidecar.write_text("class Sidecar:\n    cwd: str = None\n")
    r = subprocess.run(
        ["bash", str(GATE), str(poisoned)], capture_output=True, text=True
    )
    assert r.returncode == 1
    assert "join key" in r.stderr
