"""The suite must never resolve the identity of the session running it (x-c932).

``resolve_harness_identity`` / ``current_session_id`` read ambient env markers, so
a suite run from inside a live claude or codex session resolved THAT session: the
self-stamp and mail paths then stamped a real handle and a real model where the
hermetic expectation was ``"fno"`` / ``"unknown"``. ``conftest.py`` scrubs the
markers at module load; these tests are the proof it actually fires, on a run
where the markers are genuinely present.

The scrub covering every marker is what makes it a guard rather than a decoration,
which is why the parent test exports the WHOLE marker set rather than the two the
original bug happened to name.
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from fno.harness_identity import (
    HARNESS_SESSION_MARKERS,
    LEGACY_HARNESS_SESSION_MARKERS,
    current_session_id,
    current_session_ids,
    resolve_harness_identity,
)

ALL_MARKERS = tuple(
    marker for marker, _ in (*HARNESS_SESSION_MARKERS, *LEGACY_HARNESS_SESSION_MARKERS)
)

_CHILD_TEST = "test_no_ambient_harness_identity_resolves"


def test_no_ambient_harness_identity_resolves() -> None:
    """No harness identity is resolvable from inside the suite.

    Passes vacuously on a clean box, which is exactly why the subprocess test
    below re-runs this one with every marker exported. Keep both.
    """
    survivors = [m for m in ALL_MARKERS if m in os.environ]
    assert not survivors, f"conftest left ambient harness markers set: {survivors}"
    identity = resolve_harness_identity()
    assert identity.session_id is None and identity.harness is None
    assert current_session_id() is None
    assert current_session_ids() == set()


def test_scrub_fires_when_markers_are_actually_present() -> None:
    """Re-run the test above in a child that exports every marker.

    A green parent run proves nothing on its own: the assertion it makes is
    satisfied by an empty environment. This is the run where the markers are
    really there, so a scrub that misses one fails here and nowhere else.
    """
    repo_root = Path(__file__).resolve().parents[2]
    env = dict(os.environ)
    # Distinct values so a failure message names which marker leaked through.
    for index, marker in enumerate(ALL_MARKERS):
        env[marker] = f"0199c932-0000-7000-8000-00000000{index:04d}"

    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            f"cli/tests/{Path(__file__).name}::{_CHILD_TEST}",
            "-q",
            "-p",
            "no:randomly",
        ],
        cwd=repo_root,
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
    )

    assert result.returncode == 0, (
        "conftest's ambient scrub did not cover every harness marker:\n"
        f"{result.stdout}\n{result.stderr}"
    )
