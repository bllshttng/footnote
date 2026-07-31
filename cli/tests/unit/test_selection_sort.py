"""Unit tests for epics-first selection precedence (C3, ab-82e65b72).

`make_selection_sort_key` is the single source of truth for the
"epics-first, then flat priority" precedence (Locked Decision 7). It is
used by both `fno backlog next`/`ready` and the parallel walker's
`_select_ready_nodes`.
"""
from fno.graph._intake import make_effective_priority, make_selection_sort_key


def _ids_sorted(entries, candidates):
    key = make_selection_sort_key(entries)
    return [e["id"] for e in sorted(candidates, key=key)]


def test_epic_child_outranks_higher_priority_loose_node():
    # A p3 epic child must come before a p0 loose node: epics-first beats
    # raw priority (Locked Decision 7).
    epic = {"id": "epic1", "type": "epic", "priority": "p2", "created_at": "2026-01-01"}
    child = {"id": "child1", "parent": "epic1", "priority": "p3",
             "created_at": "2026-01-02"}
    loose = {"id": "loose1", "priority": "p0", "created_at": "2026-01-03"}
    entries = [epic, child, loose]
    assert _ids_sorted(entries, [loose, child]) == ["child1", "loose1"]


def test_higher_priority_epic_children_first():
    epic_a = {"id": "epicA", "type": "epic", "priority": "p1", "created_at": "2026-01-01"}
    epic_b = {"id": "epicB", "type": "epic", "priority": "p2", "created_at": "2026-01-01"}
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
    epic_x = {"id": "epicX", "type": "epic", "priority": "p2", "created_at": "2026-01-01"}
    epic_y = {"id": "epicY", "type": "epic", "priority": "p2", "created_at": "2026-01-01"}
    done_child = {"id": "xdone", "parent": "epicX", "priority": "p2",
                  "status": "done", "created_at": "2026-02-01"}
    ready_x = {"id": "xr", "parent": "epicX", "priority": "p2",
               "created_at": "2026-02-02"}
    ready_y = {"id": "yr", "parent": "epicY", "priority": "p2",
               "created_at": "2026-02-02"}
    entries = [epic_x, epic_y, done_child, ready_x, ready_y]
    assert _ids_sorted(entries, [ready_y, ready_x]) == ["xr", "yr"]


def test_live_claimed_child_marks_epic_in_progress_for_selection():
    epic_x = {"id": "epicX", "type": "epic", "priority": "p2",
              "created_at": "2026-02-01"}
    epic_y = {"id": "epicY", "type": "epic", "priority": "p2",
              "created_at": "2026-01-01"}
    claimed_x = {"id": "xclaimed", "parent": "epicX", "priority": "p2",
                 "status": "ready", "created_at": "2026-02-01"}
    ready_x = {"id": "xr", "parent": "epicX", "priority": "p2",
               "created_at": "2026-02-02"}
    ready_y = {"id": "yr", "parent": "epicY", "priority": "p2",
               "created_at": "2026-01-02"}
    entries = [epic_x, epic_y, claimed_x, ready_x, ready_y]

    key = make_selection_sort_key(entries, live_claimed={"xclaimed"})

    assert [e["id"] for e in sorted([ready_y, ready_x], key=key)] == ["xr", "yr"]


def test_live_claim_signal_ignores_unhashable_malformed_child_id():
    epic = {"id": "epic", "type": "epic", "priority": "p2"}
    malformed = {"id": [], "parent": "epic", "priority": "p2"}
    loose = {"id": "loose", "priority": "p2"}

    key = make_selection_sort_key(
        [epic, malformed, loose], live_claimed={"claimed"}
    )

    assert sorted([malformed, loose], key=key)


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


# -- Curated rank drives selection (AC1-*) ----------------------------------
# The selection key prepends the SAME `_rank_band` term the board uses, so a
# `fno backlog rank --top` node is worked next, not just floated on the board.

def test_ranked_node_selected_before_unranked():
    # AC1-HP: a ranked node (even at the lowest priority) is selected ahead of
    # higher-priority unranked nodes in the same project.
    ranked = {"id": "r", "priority": "p3", "created_at": "2026-03-01",
              "rank": 1.0}
    loose_a = {"id": "a", "priority": "p0", "created_at": "2026-01-01"}
    loose_b = {"id": "b", "priority": "p1", "created_at": "2026-01-02"}
    entries = [ranked, loose_a, loose_b]
    assert _ids_sorted(entries, [loose_a, loose_b, ranked])[0] == "r"


def test_lower_rank_value_sorts_first():
    # Two ranked nodes order by ascending rank (band 0 ascending), ties would
    # fall back to the existing (priority, created_at) key.
    top = {"id": "top", "priority": "p2", "created_at": "2026-01-02",
           "rank": 1.0}
    second = {"id": "second", "priority": "p0", "created_at": "2026-01-01",
              "rank": 2.0}
    entries = [top, second]
    assert _ids_sorted(entries, [second, top]) == ["top", "second"]


def test_ranked_loose_node_overrides_in_progress_epic_and_clear_restores():
    # AC1-FR: an explicit rank beats the epics-first heuristic; clearing the
    # rank restores epics-first selection.
    epic = {"id": "epic1", "type": "epic", "priority": "p1", "created_at": "2026-01-01"}
    done_child = {"id": "dc", "parent": "epic1", "priority": "p1",
                  "status": "done", "created_at": "2026-02-01"}
    ready_child = {"id": "rc", "parent": "epic1", "priority": "p1",
                   "created_at": "2026-02-02"}
    ranked_loose = {"id": "rl", "priority": "p3", "created_at": "2026-03-01",
                    "rank": 1.0}
    entries = [epic, done_child, ready_child, ranked_loose]
    # ranked loose beats the in-progress epic's ready child
    assert _ids_sorted(entries, [ready_child, ranked_loose])[0] == "rl"
    # after clearing rank (no `rank` key), the epic child wins again
    cleared = {k: v for k, v in ranked_loose.items() if k != "rank"}
    entries2 = [epic, done_child, ready_child, cleared]
    assert _ids_sorted(entries2, [ready_child, cleared])[0] == "rc"


def test_poisoned_rank_degrades_to_unranked():
    # AC1-ERR: NaN / inf / bool ranks degrade to unranked via the shared
    # `_rank_band` guard - no crash, and a high-priority unranked node wins.
    for bad in (float("nan"), float("inf"), float("-inf"), True):
        poisoned = {"id": "p", "priority": "p3", "created_at": "2026-03-01",
                    "rank": bad}
        loose = {"id": "l", "priority": "p0", "created_at": "2026-01-01"}
        entries = [poisoned, loose]
        assert _ids_sorted(entries, [poisoned, loose])[0] == "l"


def test_rank_band_is_single_source():
    # Locked Decision 4: board and selection MUST share one `_rank_band`
    # helper so they can never drift. Both import the same object.
    from fno.graph._intake import _rank_band as intake_band
    from fno.graph.render import _rank_band as render_band
    assert intake_band is render_band


def test_swimlane_mode_preserves_work_order_inside_each_project():
    """The board may group projects, but its per-project suffix is work order."""
    epic = {
        "id": "epic", "type": "epic", "project": "fno",
        "priority": "p1", "created_at": "2026-01-01",
    }
    child = {
        "id": "child", "parent": "epic", "project": "fno",
        "priority": "p1", "created_at": "2026-03-01",
    }
    loose = {
        "id": "loose", "project": "fno", "priority": "p1",
        "created_at": "2026-01-01",
    }
    web = {
        "id": "web", "project": "web", "priority": "p0",
        "created_at": "2026-01-01",
    }
    entries = [epic, child, loose, web]

    work_key = make_selection_sort_key(entries)
    board_key = make_selection_sort_key(entries, swimlane=True)

    assert [e["id"] for e in sorted([loose, child], key=work_key)] == ["child", "loose"]
    assert [e["id"] for e in sorted([web, loose, child], key=board_key)] == [
        "child", "loose", "web",
    ]


def test_terminal_epic_child_sorts_exactly_like_a_loose_node():
    for status in ("done", "superseded", "deferred"):
        epic = {
            "id": "epic", "type": "epic", "status": status,
            "priority": "p0", "created_at": "2026-01-01",
        }
        child = {
            "id": "child", "parent": "epic", "priority": "p2",
            "created_at": "2026-02-01",
        }
        loose_equivalent = {**child, "id": "loose", "parent": None}
        key = make_selection_sort_key([epic, child, loose_equivalent])
        assert key(child) == key(loose_equivalent), status


def test_terminal_epic_markers_override_stale_ready_status():
    for marker, value in (
        ("completed_at", "2026-01-02"),
        ("superseded_by", "replacement"),
        ("deferred_at", "2026-01-02"),
        ("completed_at", 1),
        ("superseded_by", True),
        ("deferred_at", {"malformed": True}),
    ):
        epic = {
            "id": "epic", "type": "epic", "status": "ready",
            "priority": "p0", marker: value,
        }
        child = {"id": "child", "parent": "epic", "priority": "p2"}
        loose_equivalent = {**child, "id": "loose", "parent": None}
        key = make_selection_sort_key([epic, child, loose_equivalent])

        assert key(child) == key(loose_equivalent), marker
        assert make_effective_priority([epic, child])(child) == "p2", marker


def test_live_epic_effective_priority_only_promotes_children():
    epic = {
        "id": "epic", "type": "epic", "status": "ready",
        "priority": "p1", "created_at": "2026-01-01",
    }
    lower_child = {"id": "lower", "parent": "epic", "priority": "p2"}
    higher_child = {"id": "higher", "parent": "epic", "priority": "p0"}
    priority_for = make_effective_priority([epic, lower_child, higher_child])

    assert priority_for(lower_child) == "p1"
    assert priority_for(higher_child) == "p0"


def test_completed_epic_does_not_confer_effective_priority():
    epic = {
        "id": "epic", "type": "epic", "status": "ready",
        "completed_at": "2026-01-01", "priority": "p0",
    }
    child = {"id": "child", "parent": "epic", "priority": "p2"}
    assert make_effective_priority([epic, child])(child) == "p2"


def test_malformed_epic_enrichment_degrades_to_safe_defaults():
    epic = {
        "id": "epic", "type": "epic", "status": [],
        "priority": [], "created_at": [],
    }
    child = {
        "id": "child", "parent": "epic", "priority": [],
        "created_at": [],
    }
    loose = {
        "id": "loose", "parent": ["not", "hashable"], "priority": "p2",
        "created_at": "",
    }

    key = make_selection_sort_key([epic, child, loose])

    assert key(child) == key(loose)
    assert make_effective_priority([epic, child])(child) == "p2"


def test_each_malformed_epic_ordering_field_makes_child_loose_equivalent():
    for field, value in (
        ("priority", []),
        ("status", []),
        ("status", "bogus"),
        ("created_at", []),
    ):
        epic = {
            "id": "epic", "type": "epic", "status": "ready",
            "priority": "p1", "created_at": "2026-01-01",
            field: value,
        }
        child = {
            "id": "child", "parent": "epic", "priority": "p2",
            "created_at": "2026-02-01",
        }
        loose = {**child, "id": "loose", "parent": None}

        key = make_selection_sort_key([epic, child, loose])

        assert key(child) == key(loose), field
        assert make_effective_priority([epic, child])(child) == "p2", field
