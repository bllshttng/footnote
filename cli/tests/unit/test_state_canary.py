from __future__ import annotations

from fno.paths import graph_json

STATE_LEAK_CANARY = "STATE_LEAK_CANARY"


def test_state_canary_detects_populated_state():
    from fno.graph.store import read_graph

    found = any(
        entry.get("id") == STATE_LEAK_CANARY for entry in read_graph(graph_json())
    )
    assert not found, (
        f"{STATE_LEAK_CANARY} reached a test process. In the populated lane "
        "this expected failure proves the state lane can detect a leak; in the "
        "clean lane it means the sandbox was populated unexpectedly."
    )
