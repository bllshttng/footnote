"""Unit tests for epics-first selection precedence (C3, ab-82e65b72).

`make_selection_sort_key` is the single source of truth for the
"epics-first, then flat priority" precedence (Locked Decision 7). It is
used by both `fno backlog next`/`ready` and the parallel walker's
`_select_ready_nodes`.
"""
from fno.graph._intake import make_selection_sort_key


def _ids_sorted(entries, candidates):
    key = make_selection_sort_key(entries)
    return [e["id"] for e in sorted(candidates, key=key)]


def test_epic_child_outranks_higher_priority_loose_node():
    # A p3 epic child must come before a p0 loose node: epics-first beats
    # raw priority (Locked Decision 7).
    epic = {"id": "epic1", "priority": "p2", "created_at": "2026-01-01"}
    child = {"id": "child1", "parent": "epic1", "priority": "p3",
             "created_at": "2026-01-02"}
    loose = {"id": "loose1", "priority": "p0", "created_at": "2026-01-03"}
    entries = [epic, child, loose]
    assert _ids_sorted(entries, [loose, child]) == ["child1", "loose1"]


def test_higher_priority_epic_children_first():
    epic_a = {"id": "epicA", "priority": "p1", "created_at": "2026-01-01"}
    epic_b = {"id": "epicB", "priority": "p2", "created_at": "2026-01-01"}
    child_a = {"id": "ca", "parent": "epicA", "priority": "p2",
               "created_at": "2026-02-01"}
    child_b = {"id": "cb", "parent": "epicB", "priority": "p0",
               "created_at": "2026-02-01"}
    entries = [epic_a, epic_b, child_a, child_b]
    # epicA (p1) outranks epicB (p2), so its child comes first even though
    # child_b is p0.
    assert _ids_sorted(entries, [child_b, child_a]) == ["ca", "cb"]


def test_in_progress_epic_preferred_over_unstarted_same_priority():
    # Two same-priority epics; epicX has a done child (in progress), epicY
    # does not. Stay focused: drain the in-progress epic first.
    epic_x = {"id": "epicX", "priority": "p2", "created_at": "2026-01-01"}
    epic_y = {"id": "epicY", "priority": "p2", "created_at": "2026-01-01"}
    done_child = {"id": "xdone", "parent": "epicX", "priority": "p2",
                  "_status": "done", "created_at": "2026-02-01"}
    ready_x = {"id": "xr", "parent": "epicX", "priority": "p2",
               "created_at": "2026-02-02"}
    ready_y = {"id": "yr", "parent": "epicY", "priority": "p2",
               "created_at": "2026-02-02"}
    entries = [epic_x, epic_y, done_child, ready_x, ready_y]
    assert _ids_sorted(entries, [ready_y, ready_x]) == ["xr", "yr"]


def test_loose_nodes_flat_priority_then_created_at():
    a = {"id": "a", "priority": "p2", "created_at": "2026-01-02"}
    b = {"id": "b", "priority": "p0", "created_at": "2026-01-03"}
    c = {"id": "c", "priority": "p2", "created_at": "2026-01-01"}
    entries = [a, b, c]
    # p0 first; among p2, earlier created_at first.
    assert _ids_sorted(entries, [a, b, c]) == ["b", "c", "a"]


def test_missing_parent_treated_as_loose():
    # parent points at an id not in the graph -> treat as a loose node,
    # never crash.
    orphan = {"id": "o", "parent": "ghost", "priority": "p1",
              "created_at": "2026-01-01"}
    loose = {"id": "l", "priority": "p1", "created_at": "2026-01-02"}
    entries = [orphan, loose]
    # both loose-equivalent; earlier created_at wins.
    assert _ids_sorted(entries, [loose, orphan]) == ["o", "l"]
