"""The undispatched queue and the ready list share one ordering (AC2)."""
from __future__ import annotations

import pytest

from fno.backlog.undispatched import classify_planned_unclaimed
from fno.graph._intake import make_selection_sort_key


def _node(node_id: str, **over) -> dict:
    entry = {
        "id": node_id,
        "title": node_id,
        "status": "ready",
        "priority": "p2",
        "project": "fno",
        "plan_path": f"plans/{node_id}.md",
        "created_at": "2026-01-01T00:00:00+00:00",
    }
    entry.update(over)
    return entry


def _ids(entries: list[dict]) -> list[str]:
    receipt = classify_planned_unclaimed(entries, [])
    return [row["id"] for row in receipt["rows"]]


def _ready_order(entries: list[dict]) -> list[str]:
    """The order the drain sees: the ready list, sorted by the shared key."""
    candidates = [e for e in entries if e.get("status") == "ready"]
    candidates.sort(key=make_selection_sort_key(entries))
    return [e["id"] for e in candidates]


def test_ac2_hp_pinned_node_leads_the_undispatched_queue():
    """A pin used to be invisible here: the queue sorted by (priority, id)."""
    entries = [
        _node("x-aaa1", priority="p0"),
        _node("x-bbb2", priority="p2", rank=-9.0),
    ]

    assert _ids(entries) == ["x-bbb2", "x-aaa1"]


def test_ac2_hp_hook_next_equals_drain_next():
    """Both queues name the same node, because they read the same key."""
    entries = [
        _node("x-fff9", priority="p1"),
        _node("x-aaa1", priority="p1", rank=-3.0),
        _node("x-ccc3", priority="p0"),
        _node("x-ddd4", priority="p3"),
    ]

    assert _ids(entries) == _ready_order(entries)
    assert _ids(entries)[0] == "x-aaa1"


def test_ac2_hp_unranked_rows_keep_priority_order():
    """With nothing pinned the queue still leads with the highest priority."""
    entries = [
        _node("x-aaa1", priority="p3"),
        _node("x-bbb2", priority="p0"),
        _node("x-ccc3", priority="p2"),
    ]

    assert _ids(entries) == ["x-bbb2", "x-ccc3", "x-aaa1"]


def test_ac2_hp_order_is_stable_across_runs():
    """Two runs against one graph render identically (id breaks every tie)."""
    entries = [_node(f"x-{i:04x}") for i in range(6)]

    assert _ids(entries) == _ids(list(reversed(entries)))
