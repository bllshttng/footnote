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


def test_mission_aggregates_child_epic_leaves():
    """AC3: a mission folds its child epic's leaves plus its own direct leaves."""
    entries = [
        _n("M", type_="epic"),                  # mission (parent null)
        _n("E", parent="M", type_="epic"),      # child epic
        _n("e1", parent="E", status="done"),    # E's 3 leaves, 1 done
        _n("e2", parent="E", status="ready"),
        _n("e3", parent="E", status="ready"),
        _n("L", parent="M", status="ready"),    # M's direct leaf
    ]
    m = compute_rollup("M", entries)
    assert m["children_total"] == 4  # 3 (E's leaves) + 1 (L), NOT counting E itself
    assert m["children_done"] == 1
    assert m["progress"] == "1/4"

    e = compute_rollup("E", entries)
    assert e["children_total"] == 3
    assert e["children_done"] == 1
    assert e["progress"] == "1/3"


def test_epic_parent_cycle_terminates():
    """A malformed epic-parent cycle terminates instead of recursing forever."""
    entries = [
        _n("A", parent="B", type_="epic"),
        _n("B", parent="A", type_="epic"),
    ]
    r = compute_rollup("A", entries)  # must return, not hang
    assert r["progress"] == "0/0"
