"""Regression: missions (top-level epics) stay non-dispatchable (x-6c2b wave 3).

A mission is a `type: epic` node with epic children, so it is some node's
parent and the existing container filter already drops it from every work
selection surface. This pins that no-code-change guarantee.
"""
from __future__ import annotations

from fno.graph.cli import _container_ids


def test_mission_and_nested_epic_are_containers():
    entries = [
        {"id": "M", "type": "epic", "parent": None},   # mission
        {"id": "E", "type": "epic", "parent": "M"},    # child epic
        {"id": "leaf", "type": "feature", "parent": "E"},
    ]
    containers = _container_ids(entries)
    assert "M" in containers  # mission never dispatched
    assert "E" in containers  # nested epic never dispatched
    assert "leaf" not in containers  # the leaf is the buildable unit
