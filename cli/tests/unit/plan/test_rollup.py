"""Unit tests for epic progress rollup counters (x-6c2b wave 2)."""
from __future__ import annotations

from fno.plan._rollup import compute_rollup


def _n(nid, parent=None, status="ready", type_="feature"):
    return {"id": nid, "parent": parent, "_status": status, "type": type_}


def test_direct_children_counted_by_status():
    """AC2: an epic's counters reflect its direct children's statuses."""
    entries = [
        _n("e", type_="epic"),
        _n("c1", parent="e", status="done"),
        _n("c2", parent="e", status="claimed"),
        _n("c3", parent="e", status="blocked"),
        _n("c4", parent="e", status="ready"),
        _n("other", parent="x"),  # not a child of e
    ]
    r = compute_rollup("e", entries)
    assert r == {
        "children_total": 4,
        "children_done": 1,
        "children_in_flight": 1,
        "children_blocked": 1,
        "progress": "1/4",
    }


def test_in_review_counts_as_in_flight():
    entries = [_n("e", type_="epic"), _n("c1", parent="e", status="in_review")]
    r = compute_rollup("e", entries)
    assert r["children_in_flight"] == 1
    assert r["progress"] == "0/1"


def test_childless_epic_zeroes():
    """AC2-EDGE: a childless epic renders 0/0, never a crash or absent key."""
    r = compute_rollup("e", [_n("e", type_="epic")])
    assert r == {
        "children_total": 0,
        "children_done": 0,
        "children_in_flight": 0,
        "children_blocked": 0,
        "progress": "0/0",
    }
