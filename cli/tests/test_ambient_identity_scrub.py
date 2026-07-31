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
import re
import subprocess
import sys
from pathlib import Path

import pytest

from fno.harness_identity import (
    AMBIENT_IDENTITY_ENV,
    current_session_id,
    current_session_ids,
    resolve_harness_identity,
)

ALL_MARKERS = AMBIENT_IDENTITY_ENV

_CHILD_TEST = "test_no_ambient_harness_identity_resolves"

# Both pytest roots. cli/src/fno/ has its own conftest, so a scrub in
# cli/tests/conftest.py protects nothing there; each tree is exercised with the
# markers really set, since covering one is what made the original guard
# decorative.
_TREES = ("cli/tests", "cli/src/fno")


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


def test_inventory_covers_every_session_env_the_source_reads_directly() -> None:
    """AMBIENT_IDENTITY_ENV must list every session marker source code reads.

    The subprocess tests below export ALL_MARKERS, which IS the inventory, so
    they cannot notice a marker missing from it - shrink the inventory and the
    exported set shrinks with it, and both stay green. This test is the
    independent half: it reads the SOURCE rather than the constant, so a module
    that reaches for a session env name nobody scrubs fails here.

    Scoped to names ending in ``_SESSION_ID`` and not footnote's own ``FNO_*``
    plumbing, which is the shape of a harness identity marker.
    """
    src = Path(__file__).resolve().parents[2] / "cli" / "src" / "fno"
    pattern = re.compile(r'environ(?:\.get)?[\(\[]"([A-Z_]+_SESSION_ID)"')

    found: dict[str, str] = {}
    for path in src.rglob("*.py"):
        if path.name.startswith("test_") or path.name == "conftest.py":
            continue
        for name in pattern.findall(path.read_text()):
            if not name.startswith("FNO_"):
                found.setdefault(name, str(path.relative_to(src)))

    assert found, "source scan found no session env reads; the pattern has rotted"
    missing = {n: where for n, where in found.items() if n not in AMBIENT_IDENTITY_ENV}
    assert not missing, (
        "these session markers are read by source but never scrubbed, so a test "
        f"or preflight leg resolves the live session through them: {missing}"
    )


@pytest.mark.parametrize("tree", _TREES)
def test_scrub_fires_when_markers_are_actually_present(tree: str) -> None:
    """Re-run the test above in a child that exports every marker.

    A green parent run proves nothing on its own: the assertion it makes is
    satisfied by an empty environment. This is the run where the markers are
    really there, so a scrub that misses one fails here and nowhere else.

    Parametrized over both pytest roots. The child test file lives under
    cli/tests/, so it is copied into the other tree to be collected by THAT
    tree's conftest - running it from cli/tests/ would only ever re-prove the
    cli/tests/ scrub.
    """
    repo_root = Path(__file__).resolve().parents[2]
    env = dict(os.environ)
    # Distinct values so a failure message names which marker leaked through.
    for index, marker in enumerate(ALL_MARKERS):
        env[marker] = f"0199c932-0000-7000-8000-00000000{index:04d}"

    target = Path(f"{tree}/{Path(__file__).name}")
    copied = tree != "cli/tests"
    if copied:
        (repo_root / target).write_text(Path(__file__).read_text())
    try:
        result = subprocess.run(
            [sys.executable, "-m", "pytest", f"{target}::{_CHILD_TEST}", "-q",
             "-p", "no:randomly"],
            cwd=repo_root,
            env=env,
            capture_output=True,
            text=True,
            timeout=300,
        )
    finally:
        if copied:
            (repo_root / target).unlink(missing_ok=True)

    assert result.returncode == 0, (
        f"{tree} does not scrub every ambient identity marker:\n"
        f"{result.stdout}\n{result.stderr}"
    )
