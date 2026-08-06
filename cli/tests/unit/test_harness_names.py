"""x-cec8: the harness-name set is platform-layer data, and importing the
platform layer no longer drags the runtime in at import time."""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def test_known_harnesses_matches_capability_table_keys():
    """Adding a harness is a coupled change: the name lands in KNOWN_HARNESSES
    and a capability dict lands in _HARNESS_CAPS, or the table's import-time
    assertion fires. Pin that contract so a one-sided add is caught here too."""
    from fno.agents.harness_map import _HARNESS_CAPS, known_harnesses
    from fno.harness_names import KNOWN_HARNESSES

    assert set(known_harnesses()) == set(KNOWN_HARNESSES)
    assert set(_HARNESS_CAPS) == set(KNOWN_HARNESSES)
    assert known_harnesses() == sorted(known_harnesses())


def test_importing_dispatch_flags_does_not_drag_the_runtime():
    """x-cec8 defect: importing fno.dispatch_flags used to eagerly import
    fno.agents (via harness_identity -> harness_map). The name set now lives at
    L0, so the platform layer imports no runtime module at import time.

    Run in a SUBPROCESS: it makes the import genuinely fresh AND, critically,
    keeps this test from mutating the test process's sys.modules. Deleting
    fno.agents here would re-run its module body on the next import and split
    class identity (another module's AgentNameError vs a re-imported one), so a
    later test's ``pytest.raises(AgentNameError)`` would not catch the raised
    error and it would propagate. A subprocess cannot pollute the parent."""
    repo_src = Path(__file__).resolve().parents[3] / "cli" / "src"
    env = dict(os.environ, PYTHONPATH=str(repo_src))
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            "import sys, fno.dispatch_flags; "
            "print(sorted(k for k in sys.modules if k.startswith('fno.agents')) or 'NONE')",
        ],
        env=env,
        capture_output=True,
        text=True,
        check=True,
    )
    assert "NONE" in result.stdout, (
        f"dispatch_flags dragged the runtime at import: {result.stdout.strip()}"
    )

