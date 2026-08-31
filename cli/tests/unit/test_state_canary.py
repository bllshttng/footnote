from __future__ import annotations

import os
import shutil
from pathlib import Path

from fno.paths import graph_json

STATE_LEAK_CANARY = "STATE_LEAK_CANARY"


def test_state_canary_detects_populated_state():
    from fno.graph.store import read_graph

    profile = os.environ.get("STATE_PROFILE_DIR")
    if os.environ.get("STATE_LANE") == "populated" and profile:
        shutil.copytree(profile, Path.home() / ".fno", dirs_exist_ok=True)
    found = any(
        entry.get("id") == STATE_LEAK_CANARY for entry in read_graph(graph_json())
    )
    assert not found, (
        f"{STATE_LEAK_CANARY} reached a test process. In the populated lane "
        "this expected failure proves the state lane can detect a leak; in the "
        "clean lane it means the sandbox was populated unexpectedly."
    )
