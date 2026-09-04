"""The king board: a queue-empty read whose emptiness a third party can re-run.

Two regressions matter more than the rest and each has its own test below.
An unreadable queue must never collapse to an empty one, and the wire status
word must never decide that a lane is staffed.
"""
from __future__ import annotations

import json

import pytest

from fno.king.board import BoardInputs, SourceRead, build_board


def _ok(payload):
    return SourceRead(payload=payload)


def _empty_inputs(**overrides) -> BoardInputs:
    base = dict(
        ready=_ok([]),
        claims=_ok([]),
        claimed_nodes=_ok([]),
        holder_activity={},
        prs=_ok([]),
        outstanding=_ok({}),
        needs=_ok([]),
        lane=_ok([]),
    )
    base.update(overrides)
    return BoardInputs(**base)


def _lane_item(text, *, node=None, parked=None, done=False, line=1):
    return {"text": text, "node": node, "parked": parked, "done": done, "line": line}


def _node(node_id, priority="p0", plan_path="/plans/p.md"):
    return {"id": node_id, "priority": priority, "plan_path": plan_path}


def _claim(node_id, state="live", holder="target-session:abc"):
    return {"key": f"node:{node_id}", "state": state, "holder": holder}


def _held(node_id, holder, *, priority="p0", activity=None):
    """A node held by a live claim, in the shape the real verbs produce.

    The load-bearing detail is that the node is NOT in `ready`. `fno backlog
    ready` filters through `live_claimed_node_ids`, so a live-claimed node
    cannot appear there. The first cut of these tests put one in both lists,
    which is a state the system cannot reach, and the stalled_holder queue
    passed its tests while being structurally unreachable in production.
    """
    return dict(
        ready=_ok([]),
        claims=_ok([_claim(node_id, holder=holder)]),
        claimed_nodes=_ok([_node(node_id, priority=priority)]),
        holder_activity={holder: activity} if activity else {},
    )


def _queue(board, name):
    for q in board["queues"]:
        if q["name"] == name:
            return q
    raise AssertionError(f"no queue named {name}: {[q['name'] for q in board['queues']]}")


def test_scope_ids_filter_every_node_bearing_queue_and_report_rejections():
    inputs = _empty_inputs(
        ready=_ok(
            [
                _node("in-ready"),
                _node("out-ready"),
                _node("out-unplanned", plan_path=None),
            ]
        ),
        claims=_ok(
            [
                _claim("in-held", holder="target-session:in"),
                _claim("out-held", holder="target-session:out"),
                _claim("in-stale", state="stale"),
                _claim("out-stale", state="stale"),
            ]
        ),
        claimed_nodes=_ok([_node("in-held"), _node("out-held")]),
        pr_nodes=_ok([_pr_node("in-pr"), _pr_node("out-pr")]),
        needs=_ok(
            [
                {"kind": "unreachable", "name": "inside", "node": "in-ready"},
                {"kind": "unreachable", "name": "outside", "node": "out-ready"},
            ]
        ),
    )

    board = build_board(
        inputs,
        crown_scope="x-root",
        scope_ids={"in-ready", "in-held", "in-stale", "in-pr"},
    )

    assert [r["id"] for r in _queue(board, "undispatched")["rows"]] == ["in-ready"]
    assert _queue(board, "unplanned")["rows"] == []
    assert [r["id"] for r in _queue(board, "stalled_holder")["rows"]] == ["in-held"]
    assert [r["id"] for r in _queue(board, "undriven_pr")["rows"]] == ["in-pr"]
    assert [r["key"] for r in _queue(board, "stale_claim")["rows"]] == [
        "node:in-stale"
    ]
    assert [r["name"] for r in _queue(board, "unreachable_worker")["rows"]] == [
        "inside"
    ]
    outside = _queue(board, "out_of_scope")
    assert outside["actionable"] is False
    assert {r["id"] for r in outside["rows"]} == {
        "out-ready",
        "out-unplanned",
        "out-held",
        "out-stale",
        "out-pr",
    }
    assert board["actionable"] == 4


def test_a_scoped_board_demotes_the_operator_lane_to_report_only():
    """Lane lines carry no node id, so a scoped king cannot attribute them.

    Counting them would hold an epic king open on the operator's global
    priorities, including lines belonging to other projects - the exact wedge
    the scope subset rule exists to prevent. Unscoped, the lane stays the
    actionable queue it always was.
    """
    inputs = _empty_inputs(
        ready=_ok([_node("in-ready")]),
        lane=_ok([_lane_item("ship the newsletter")]),
    )

    scoped = build_board(inputs, crown_scope="x-root", scope_ids={"in-ready"})
    unscoped = build_board(inputs)

    assert _queue(scoped, "operator_lane")["actionable"] is False
    assert "report-only" in _queue(scoped, "operator_lane")["note"]
    assert _queue(unscoped, "operator_lane")["actionable"] is True


def test_epic_scope_compiles_to_the_root_and_transitive_descendants():
    from fno.king import board as board_mod

    compile_ids = getattr(board_mod, "compile_scope_ids", None)
    assert compile_ids is not None, "scope compiler is missing"
    entries = [
        {"id": "x-root", "type": "epic", "project": "fno"},
        {"id": "x-child", "parent": "x-root", "project": "fno"},
        {"id": "x-grand", "parent": "x-child", "project": "fno"},
        {"id": "x-other", "project": "fno"},
    ]

    assert compile_ids("x-root", entries, resolve=lambda _: (2, "x-root")) == {
        "x-root",
        "x-child",
        "x-grand",
    }


def test_project_and_portfolio_scope_compile_to_their_project_union():
    from fno.king import board as board_mod

    compile_ids = getattr(board_mod, "compile_scope_ids", None)
    assert compile_ids is not None, "scope compiler is missing"
    entries = [
        {"id": "f-1", "project": "fno"},
        {"id": "e-1", "project": "etl"},
        {"id": "w-1", "project": "web"},
    ]

    assert compile_ids("fno", entries, resolve=lambda _: (1, "fno")) == {"f-1"}
    assert compile_ids("etl,fno", entries, resolve=lambda _: (0, "etl,fno")) == {
        "e-1",
        "f-1",
    }


# --- AC1-HP -----------------------------------------------------------------


def test_undispatched_row_names_the_node_and_carries_a_rerunnable_source():
    board = build_board(
        _empty_inputs(undispatched=_ok([_node("x-1234")]))
    )

    q = _queue(board, "undispatched")
    assert q["actionable"] is True
    assert q["count"] == 1
    assert [r["id"] for r in q["rows"]] == ["x-1234"]
    assert q["source"].startswith("fno backlog undispatched --json")
    assert board["actionable"] == 1


def test_every_queue_carries_a_source_command():
    board = build_board(_empty_inputs())
    assert len(board["queues"]) == 11
    for q in board["queues"]:
        assert q["source"], f"{q['name']} has no source command"


def test_p2_node_is_not_undispatched_work():
    board = build_board(
        _empty_inputs(undispatched=_ok([_node("x-1234", priority="p2")]))
    )
    assert _queue(board, "undispatched")["count"] == 0
    assert board["actionable"] == 0


def test_node_without_a_plan_is_not_undispatched_work():
    board = build_board(
        _empty_inputs(
            ready=_ok([_node("x-1234", plan_path=None)]),
            undispatched=_ok([]),
        )
    )
    assert _queue(board, "undispatched")["count"] == 0
    assert [r["id"] for r in _queue(board, "unplanned")["rows"]] == ["x-1234"]


def test_a_planless_node_is_unplanned_work_and_names_the_blueprint_verb():
    board = build_board(
        _empty_inputs(
            ready=_ok([_node("x-df08", priority="p1", plan_path=None)]),
            undispatched=_ok([]),
        )
    )

    q = _queue(board, "unplanned")
    assert [r["id"] for r in q["rows"]] == ["x-df08"]
    assert q["verb"] == "/fno:blueprint"
    assert q["actionable"] is True
    assert board["actionable"] >= 1


def test_a_planless_node_makes_the_board_actionable_so_the_king_loop_does_not_exit_nowork():
    board = build_board(
        _empty_inputs(
            ready=_ok([_node("x-df08", priority="p1", plan_path=None)]),
            undispatched=_ok([]),
        )
    )

    assert board["actionable"] == 1


def test_a_planned_node_routes_to_target_and_never_to_blueprint():
    planned = _node("x-planned", priority="p0")
    board = build_board(
        _empty_inputs(ready=_ok([planned]), undispatched=_ok([planned]))
    )

    assert [r["id"] for r in _queue(board, "undispatched")["rows"]] == ["x-planned"]
    assert _queue(board, "undispatched")["verb"] == "/fno:target"
    assert _queue(board, "unplanned")["rows"] == []


def test_a_p2_planless_node_is_not_the_kings_work():
    board = build_board(
        _empty_inputs(
            ready=_ok([_node("x-p2", priority="p2", plan_path=None)]),
            undispatched=_ok([]),
        )
    )

    assert _queue(board, "unplanned")["count"] == 0
    assert board["actionable"] == 0


@pytest.mark.parametrize("state", sorted({"stale", "corrupted"}))
def test_a_stale_claim_takes_a_planless_node_out_of_unplanned(state):
    inputs = _empty_inputs(
        ready=_ok([_node("x-stale", plan_path=None)]),
        undispatched=_ok([]),
        claims=_ok([_claim("x-stale", state=state)]),
    )
    board = build_board(inputs)

    assert _queue(board, "unplanned")["count"] == 0
    assert _queue(board, "stale_claim")["count"] == 1


def test_a_live_claimed_planless_node_is_already_out_of_ready():
    """The upstream ready reader removes live claims before the board sees them."""
    inputs = _empty_inputs(
        ready=_ok([]),
        undispatched=_ok([]),
        claims=_ok([_claim("x-live")]),
        claimed_nodes=_ok([_node("x-live", plan_path=None)]),
    )
    board = build_board(inputs)

    assert _queue(board, "unplanned")["count"] == 0


def test_an_unreadable_ready_read_makes_unplanned_unreadable_never_empty():
    board = build_board(
        _empty_inputs(
            ready=SourceRead(error="exit 1: boom"),
            undispatched=_ok([]),
        )
    )

    q = _queue(board, "unplanned")
    assert q["status"] == "unreadable"
    assert q["count"] is None
    assert board["unreadable"] >= 1
    assert board["exit_code"] == 1
    assert q["verb"] == "/fno:blueprint"


@pytest.mark.parametrize("status", ["deferred", "blocked", "in_progress", "done"])
def test_only_an_idea_status_admits_a_planless_node(status):
    """The king's unplanned queue relies on ready's cold-dispatch gate."""
    from fno.graph.ladder import is_cold_dispatchable

    assert not is_cold_dispatchable({"id": "x-1", "status": status})
    assert is_cold_dispatchable({"id": "x-1", "status": "idea"})


def test_a_claim_of_any_state_takes_a_node_out_of_undispatched():
    inputs = _empty_inputs(
        undispatched=_ok([_node("x-1234")]),
        claims=_ok([_claim("x-1234", state="stale")]),
    )
    board = build_board(inputs)
    assert _queue(board, "undispatched")["count"] == 0
    assert _queue(board, "stale_claim")["count"] == 1


def test_ac2_err_unreadable_observer_blocks_clean_board():
    board = build_board(
        _empty_inputs(undispatched=SourceRead(error="graph unreadable: entries missing"))
    )

    q = _queue(board, "undispatched")
    assert q["status"] == "unreadable"
    assert q["count"] is None
    assert board["actionable"] >= 1


def test_collect_observer_unwraps_receipt_rows(monkeypatch):
    from fno.king import board as board_mod

    node = _node("x-receipt")
    monkeypatch.setattr(
        board_mod,
        "_run_json",
        lambda *args, **kwargs: _ok(
            {"status": "ok", "entries_scanned": 1, "rows": [node]}
        ),
    )

    read = board_mod._read_undispatched(timeout=1)

    assert read.ok
    assert read.rows() == [node]


def test_collect_observer_rejects_unreadable_receipt(monkeypatch):
    from fno.king import board as board_mod

    monkeypatch.setattr(board_mod, "_run_json", lambda *args, **kwargs: _ok([]))

    read = board_mod._read_undispatched(timeout=1)

    assert not read.ok
    assert "receipt" in read.error


# --- AC1-EDGE ---------------------------------------------------------------


def test_human_gated_queues_report_their_count_without_gating_nowork():
    inputs = _empty_inputs(
        outstanding=_ok({"questions": [{"id": "q-1", "question": "which lane?"}]}),
        needs=_ok([{"kind": "unreachable", "name": "w-1"}]),
    )
    board = build_board(inputs)

    assert board["actionable"] == 0
    for name in ("operator_question", "unreachable_worker"):
        q = _queue(board, name)
        assert q["count"] == 1
        assert q["actionable"] is False
        assert q["note"], f"{name} must say why the king cannot clear it"


def test_operator_question_rows_come_from_outstanding_not_from_agents_needs():
    """`fno agents needs` also emits kind=operator_question rows; counting both
    double-reports the same human queue."""
    inputs = _empty_inputs(
        needs=_ok([{"kind": "operator_question", "name": "x-3a91"}]),
    )
    board = build_board(inputs)
    assert _queue(board, "operator_question")["count"] == 0
    assert _queue(board, "unreachable_worker")["count"] == 0


# --- AC1b-EDGE --------------------------------------------------------------


@pytest.mark.parametrize(
    "autonomous_merge,expected_actionable,expected_total",
    [(False, False, 0), (True, True, 1)],
)
def test_mergeable_pr_is_gated_on_the_autonomous_merge_key(
    autonomous_merge, expected_actionable, expected_total
):
    inputs = _empty_inputs(prs=_ok([{"number": 920, "title": "t"}]))
    board = build_board(inputs, autonomous_merge=autonomous_merge)

    q = _queue(board, "mergeable_pr")
    assert q["count"] == 1
    assert q["actionable"] is expected_actionable
    assert board["actionable"] == expected_total


# --- AC1c-HP ----------------------------------------------------------------


def test_a_live_claimed_node_is_never_in_the_ready_list():
    """The fixture invariant every stalled_holder test below depends on.

    `fno backlog ready` filters through `live_claimed_node_ids`. Measured
    2026-08-18: 12 live-or-suspect node claims on one machine, zero of them in
    the ready payload. A test that puts a live-claimed node in `ready` pins a
    state the system cannot reach and reports coverage over an unreachable
    path, which is how this queue passed its tests while being dead code.
    """
    inputs = _empty_inputs(**_held("x-1234", "target-session:dead"))
    assert inputs.ready.rows() == []
    assert [n["id"] for n in inputs.claimed_nodes.rows()] == ["x-1234"]


def test_node_held_by_an_inactive_holder_is_stalled():
    inputs = _empty_inputs(
        **_held(
            "x-1234",
            "target-session:dead",
            activity={"state": "stalled", "age_s": 9000},
        )
    )
    board = build_board(inputs)

    assert [r["id"] for r in _queue(board, "stalled_holder")["rows"]] == ["x-1234"]
    assert _queue(board, "undispatched")["count"] == 0
    assert board["actionable"] == 1


def test_node_held_by_an_active_holder_appears_in_neither_queue():
    inputs = _empty_inputs(
        **_held(
            "x-1234", "target-session:busy", activity={"state": "working", "age_s": 12}
        )
    )
    board = build_board(inputs)

    assert _queue(board, "stalled_holder")["count"] == 0
    assert _queue(board, "undispatched")["count"] == 0
    assert board["actionable"] == 0


def test_an_active_state_with_a_stale_age_is_still_stalled():
    """`working` with a nine-hour-old transcript is the wedged worker; the age is
    what makes the activity axis actionable rather than decorative."""
    inputs = _empty_inputs(
        **_held(
            "x-1234",
            "target-session:wedged",
            activity={"state": "working", "age_s": 60 * 60 * 9},
        )
    )
    board = build_board(inputs)
    assert _queue(board, "stalled_holder")["count"] == 1


def test_unresolvable_holder_activity_is_stalled_not_staffed():
    inputs = _empty_inputs(**_held("x-1234", "target-session:ghost"))
    board = build_board(inputs)
    assert _queue(board, "stalled_holder")["count"] == 1


def test_a_p2_node_held_by_a_wedged_worker_is_not_the_kings_work():
    inputs = _empty_inputs(
        **_held("x-1234", "target-session:dead", priority="p2")
    )
    board = build_board(inputs)
    assert _queue(board, "stalled_holder")["count"] == 0


# --- AC1c-ERR ---------------------------------------------------------------


def test_a_holder_whose_wire_status_reads_live_is_still_stalled_when_parked():
    """The specific regression this file exists to catch.

    `WIRE_STATUS` renders working, parked and model-refused all as `live`, so a
    board that asked `status == "live"` would call this lane staffed. The
    fixture pins `status` to exactly `live` so the wrong read fails loudly.
    """
    inputs = _empty_inputs(
        **_held(
            "x-1234",
            "target-session:parked",
            activity={
                "state": "done",
                "age_s": 30,
                "status": "live",
                "reachability": "reachable",
            },
        )
    )
    board = build_board(inputs)

    assert [r["id"] for r in _queue(board, "stalled_holder")["rows"]] == ["x-1234"]
    assert board["actionable"] == 1


def test_the_board_module_never_compares_against_the_wire_status_word():
    """Verification step 2 of the plan, asserted rather than left to a reviewer."""
    import inspect

    from fno.king import board as board_mod

    src = inspect.getsource(board_mod)
    for forbidden in ('"live"', "'live'", "WIRE_STATUS"):
        assert forbidden not in src, f"board.py compares against {forbidden}"


def test_the_active_state_set_is_imported_not_copied():
    """x-cbd9 owns the roster vocabulary; a copied set would not inherit its
    fourth word."""
    from fno.agents.reachability import _ACTIVE_STATES
    from fno.king import board as board_mod

    assert board_mod.ACTIVE_STATES is _ACTIVE_STATES


# --- the driver predicate (one answer, two queues) ---------------------------


def test_node_driver_answers_active_for_a_live_claim_with_a_fresh_holder():
    from fno.king.board import _node_driver

    claim = _claim("x-live", holder="target-session:busy")
    state, found = _node_driver(
        "x-live",
        {"x-live": claim},
        {"target-session:busy": {"state": "working", "age_s": 12}},
    )
    assert state == "active"
    assert found is claim


def test_node_driver_answers_stalled_for_a_live_claim_with_no_active_holder():
    from fno.king.board import _node_driver

    claim = _claim("x-quiet", holder="target-session:ghost")
    state, found = _node_driver("x-quiet", {"x-quiet": claim}, {})
    assert state == "stalled"
    assert found is claim


def test_node_driver_answers_none_for_an_absent_or_dead_claim():
    """``none`` covers both an absent claim (the worker exited cleanly and
    released) and a dead one (the lock outlived its holder). The dead half
    belongs to `stale_claim` too; the caller filters further."""
    from fno.king.board import _DEAD_CLAIM_STATES, _node_driver

    assert _node_driver("x-gone", {}, {}) == ("none", None)
    for state in sorted(_DEAD_CLAIM_STATES):
        claim = _claim("x-dead", state=state)
        assert _node_driver("x-dead", {"x-dead": claim}, {}) == ("none", claim)


# --- AC1-ERR ----------------------------------------------------------------


def test_an_unreadable_queue_is_loud_and_never_reads_as_empty():
    inputs = _empty_inputs(ready=SourceRead(error="fno: exit 1: graph.json is corrupt"))
    board = build_board(inputs)

    q = _queue(board, "undispatched")
    assert q["status"] == "unreadable"
    assert "graph.json is corrupt" in q["error"]
    assert board["actionable"] > 0, "a blind board must never terminate NoWork"
    assert board["exit_code"] != 0


def test_an_unreadable_source_feeding_two_queues_marks_both():
    inputs = _empty_inputs(claims=SourceRead(error="claims root unreadable"))
    board = build_board(inputs)

    for name in ("undispatched", "stalled_holder", "stale_claim"):
        assert _queue(board, name)["status"] == "unreadable", name
    assert board["exit_code"] != 0


def test_an_unreadable_report_only_queue_is_loud_but_still_never_gates_nowork():
    """`fno outstanding` has been measured timing out on this machine. Counting
    a blind human-gated queue as work would wedge the loop on the one queue the
    locked decision says must never wedge it."""
    inputs = _empty_inputs(outstanding=SourceRead(error="timed out after 60s"))
    board = build_board(inputs)

    assert _queue(board, "operator_question")["status"] == "unreadable"
    assert board["exit_code"] != 0
    assert board["actionable"] == 0


def test_a_healthy_board_exits_zero_and_is_json_serialisable():
    board = build_board(_empty_inputs())
    assert board["actionable"] == 0
    assert board["exit_code"] == 0
    json.dumps(board)


# --- the operator lane -------------------------------------------------------


def test_operator_lane_is_the_first_queue_above_undispatched():
    """AC2-HP: the lane outranks the agent queue in the payload, not just prose."""
    inputs = _empty_inputs(lane=_ok([_lane_item("ship 981 and 971 tonight")]))
    board = build_board(inputs)

    assert board["queues"][0]["name"] == "operator_lane"
    assert board["queues"][0]["count"] == 1
    assert board["queues"][1]["name"] == "undispatched"
    assert board["queues"][0]["actionable"] is True


def test_a_linked_lane_item_is_absent_from_the_count():
    """AC3-HP: filing a node onto the line shrinks the lane."""
    inputs = _empty_inputs(
        lane=_ok(
            [
                _lane_item("call the dentist", line=1),
                _lane_item("ship it", node="x-aaae", line=2),
            ]
        )
    )
    board = build_board(inputs)
    q = _queue(board, "operator_lane")
    assert q["count"] == 1
    assert [r["text"] for r in q["rows"]] == ["call the dentist"]


def test_no_lane_file_reads_as_ok_and_empty():
    """AC4-EDGE: a missing lane is silence, not a broken board."""
    board = build_board(_empty_inputs())
    q = _queue(board, "operator_lane")
    assert q["status"] == "ok"
    assert q["count"] == 0


def test_unreadable_lane_is_never_reported_as_empty():
    """AC5-EDGE: an unreadable lane blinds the king; it must not read as clean."""
    inputs = _empty_inputs(lane=SourceRead(error="cannot read operator lane: permission denied"))
    board = build_board(inputs)

    q = _queue(board, "operator_lane")
    assert q["status"] == "unreadable"
    assert q["error"]
    assert board["exit_code"] != 0


def test_parked_item_is_excluded_and_counted_in_the_note():
    """AC7-HP: a reasoned park removes the item and the note says how many."""
    inputs = _empty_inputs(
        lane=_ok(
            [
                _lane_item("call the dentist", parked="not node-shaped", line=1),
                _lane_item("ship it", line=2),
            ]
        )
    )
    board = build_board(inputs)
    q = _queue(board, "operator_lane")
    assert q["count"] == 1
    assert "1 parked" in q["note"]


# --- the PR filter ----------------------------------------------------------


@pytest.mark.parametrize(
    "check,expected",
    [
        # A check run still in flight carries a NULL conclusion. Reading only
        # `conclusion` would score it as neither failed nor pending, so a PR
        # with CI running would read green and the king would merge it.
        ({"conclusion": None, "status": "IN_PROGRESS"}, 0),
        ({"conclusion": None, "status": "QUEUED"}, 0),
        ({"conclusion": "FAILURE", "status": "COMPLETED"}, 0),
        ({"state": "PENDING"}, 0),
        ({"state": "FAILURE"}, 0),
        ({"conclusion": "SUCCESS", "status": "COMPLETED"}, 1),
        ({"state": "SUCCESS"}, 1),
    ],
)
def test_only_a_finished_green_pr_counts_as_mergeable(check, expected, monkeypatch):
    from fno.king import board as board_mod

    listing = [
        {
            "number": 1,
            "title": "t",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [check],
        }
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))
    read, _, _ = board_mod._read_prs(timeout=1, max_pr_reads=10)
    assert len(read.rows()) == expected


def test_an_unmergeable_pr_is_not_the_kings_work(monkeypatch):
    from fno.king import board as board_mod

    listing = [
        {"number": 1, "title": "t", "mergeable": "CONFLICTING", "statusCheckRollup": []}
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))
    read, _, _ = board_mod._read_prs(timeout=1, max_pr_reads=10)
    assert read.rows() == []


def test_missing_bindings_warn_for_pending_red_and_green_prs(monkeypatch):
    """Hermetic: the graph read is patched at the store seam. The real
    `read_graph_strict` rides the Rust store keeper, which a CI runner
    without the built worker cannot spawn (measured on PR 1437's
    changed-smoke), and these tests subject the BINDING logic, not the
    store."""
    from fno.king import board as board_mod

    entries = [
        {"id": "x-1111", "status": "ready"},
        {"id": "x-2222", "status": "ready"},
        {"id": "x-3333", "status": "ready"},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda _path: entries)
    listing = [
        {
            "number": 1,
            "title": "pending",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [{"status": "IN_PROGRESS"}],
            "headRefName": "feature/x-1111",
            "url": "https://github.com/o/r/pull/1",
        },
        {
            "number": 2,
            "title": "red",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [{"conclusion": "FAILURE", "status": "COMPLETED"}],
            "headRefName": "feature/x-2222",
            "url": "https://github.com/o/r/pull/2",
        },
        {
            "number": 3,
            "title": "green",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [{"conclusion": "SUCCESS", "status": "COMPLETED"}],
            "headRefName": "feature/x-3333",
            "url": "https://github.com/o/r/pull/3",
        },
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))

    read, _, warnings = board_mod._read_prs(timeout=1, max_pr_reads=10)

    assert read.rows() == [{"number": 3, "title": "green"}]
    assert {"#1", "#2", "#3"} <= {
        warning.split()[1] for warning in warnings if "pr_node_binding_missing" in warning
    }


def test_a_superseded_red_beside_a_fresh_green_reads_mergeable(monkeypatch):
    """The real shape, hit twice in one night on two different PRs. A force/amend push leaves the
    superseded run's FAILURE sitting beside the fresh run's SUCCESS in the
    same statusCheckRollup, same check name. Flattening every entry's
    conclusion into one set (the pre-fix behavior) poisons that set with the
    stale FAILURE and the PR reads red even though the current run is green -
    which is what nearly made a king dispatch work twice tonight. The fix
    dedupes to the latest-started run per check name before classifying, same
    as `_status._latest_per_name`."""
    from fno.king import board as board_mod

    listing = [
        {
            "number": 917,
            "title": "t",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [
                {
                    "name": "ci",
                    "conclusion": "FAILURE",
                    "status": "COMPLETED",
                    "startedAt": "2026-08-19T00:00:00Z",
                },
                {
                    "name": "ci",
                    "conclusion": "SUCCESS",
                    "status": "COMPLETED",
                    "startedAt": "2026-08-19T00:10:00Z",
                },
            ],
        }
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))
    read, _, _ = board_mod._read_prs(timeout=1, max_pr_reads=10)
    assert [r["number"] for r in read.rows()] == [917]


# --- the PR listing bound (codex P2) ----------------------------------------
#
# `gh pr list` fetches 30 by default. An unbounded listing plus a post-hoc
# slice dropped eligible PRs from the count, and a truncated read then reached
# NoWork with an empty board. The bound has to sit on the CALL, where it costs
# something, not on the rows after they are already paid for.


def test_the_listing_names_its_own_limit(monkeypatch):
    """An implicit 30 is a truncation nobody declared."""
    from fno.king import board as board_mod

    seen: list[list[str]] = []

    def _capture(cmd, **kwargs):
        seen.append(cmd)
        return _ok([])

    monkeypatch.setattr(board_mod, "_run_json", _capture)
    board_mod._read_prs(timeout=1, max_pr_reads=7)
    assert seen, "no listing call was made"
    cmd = seen[0]
    assert "--limit" in cmd, f"listing does not bound itself: {cmd}"
    assert cmd[cmd.index("--limit") + 1] == "7"


def test_a_listing_that_hits_its_bound_is_reported(monkeypatch):
    """Exactly-at-the-bound is indistinguishable from more-were-waiting."""
    from fno.king import board as board_mod

    listing = [
        {"number": n, "title": "t", "mergeable": "MERGEABLE", "statusCheckRollup": []}
        for n in range(5)
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))
    _, _, warnings = board_mod._read_prs(timeout=1, max_pr_reads=5)
    assert any("5" in w and "mergeable_pr" in w for w in warnings), warnings


def test_every_fetched_pr_is_judged(monkeypatch):
    """Rows already fetched are free to read; discarding them drops real work.

    The old cap sliced BEFORE filtering, so a green mergeable PR past the slice
    vanished from the count while costing the same single API call.
    """
    from fno.king import board as board_mod

    listing = [
        {"number": n, "title": "t", "mergeable": "MERGEABLE", "statusCheckRollup": []}
        for n in range(8)
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))
    read, _, _ = board_mod._read_prs(timeout=1, max_pr_reads=5)
    assert len(read.rows()) == 8


# --- the open-PR node rows (undriven_pr's source) -----------------------------


def test_read_prs_carries_the_bound_node_rows_as_their_own_source(monkeypatch):
    """`fno backlog ready` removes live-claimed rows AND never returns
    in_review, so an undriven-PR queue's candidates can only come from here:
    the open-PR listing intersected with the graph. Hermetic at the store
    seam (see the missing-bindings test for why)."""
    from fno.king import board as board_mod

    entries = [{"id": "x-a0a3", "status": "in_review", "pr_number": 1394}]
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda _path: entries)
    listing = [
        {
            "number": 1394,
            "title": "t",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [{"conclusion": "SUCCESS", "status": "COMPLETED"}],
            "headRefName": "feature/x-a0a3",
            "url": "https://github.com/o/r/pull/1394",
        }
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))

    prs, pr_nodes, _warnings = board_mod._read_prs(timeout=1, max_pr_reads=10)

    rows = pr_nodes.rows()
    assert [r["id"] for r in rows] == ["x-a0a3"]
    assert rows[0]["pr_number"] == 1394
    assert rows[0]["pr_url"] == "https://github.com/o/r/pull/1394"
    assert prs.ok


def test_an_unreadable_graph_leaves_prs_readable_and_pr_nodes_unreadable(monkeypatch):
    """`mergeable_pr` survives a broken graph read because it needs no node;
    a node queue degrades to UNREADABLE, never to empty - an empty read is
    the clean-board lie this module exists against."""
    import fno.graph.store as store_mod
    from fno.king import board as board_mod

    def _boom(_path):
        raise RuntimeError("graph unreadable")

    monkeypatch.setattr(store_mod, "read_graph_strict", _boom)
    listing = [
        {
            "number": 1,
            "title": "t",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [{"conclusion": "SUCCESS", "status": "COMPLETED"}],
            "headRefName": "feature/x-1111",
            "url": "u1",
        }
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))

    prs, pr_nodes, warnings = board_mod._read_prs(timeout=1, max_pr_reads=10)

    assert prs.ok and prs.rows()
    assert not pr_nodes.ok
    assert any(w.startswith("pr_node_binding_unreadable") for w in warnings)


# --- the undriven_pr queue (a PR nobody is driving) ---------------------------


def _pr_node(node_id, *, status="in_review", pr_number=1394, priority="p1", **extra):
    row = {
        "id": node_id,
        "priority": priority,
        "status": status,
        "pr_number": pr_number,
        "pr_url": f"https://github.com/o/r/pull/{pr_number}",
        "title": f"t-{node_id}",
        "plan_path": "/plans/p.md",
    }
    row.update(extra)
    return row


def test_undriven_pr_names_the_id_and_disappears_under_a_live_claim():
    """The positive marker, by node id, in both directions. A board run that
    merely returns rows proves nothing: the board already returned 123 rows
    while missing all seven orphans."""
    node = _pr_node("x-a0a3")
    board = build_board(_empty_inputs(pr_nodes=_ok([node])))
    assert [r["id"] for r in _queue(board, "undriven_pr")["rows"]] == ["x-a0a3"]

    holder = "target-session:live"
    board = build_board(
        _empty_inputs(
            pr_nodes=_ok([node]),
            claims=_ok([_claim("x-a0a3", holder=holder)]),
            holder_activity={holder: {"state": "working", "age_s": 12}},
        )
    )
    assert _queue(board, "undriven_pr")["rows"] == []


def test_an_undriven_pr_is_partitioned_against_stalled_holder():
    """Two queues, one predicate: ``none`` goes to undriven_pr, ``stalled`` to
    stalled_holder. The same node can never be dispatched twice."""
    node = _pr_node("x-part")
    holder = "target-session:dead"
    board = build_board(
        _empty_inputs(
            pr_nodes=_ok([node]),
            claims=_ok([_claim("x-part", holder=holder)]),
            holder_activity={holder: {"state": "stalled", "age_s": 9000}},
            claimed_nodes=_ok([node]),
        )
    )
    assert [r["id"] for r in _queue(board, "stalled_holder")["rows"]] == ["x-part"]
    assert _queue(board, "undriven_pr")["rows"] == []


def test_a_deferred_node_with_an_open_pr_is_not_undriven_work():
    """Report only: a node the operator already deferred is skipped rather
    than nagged back up."""
    board = build_board(
        _empty_inputs(pr_nodes=_ok([_pr_node("x-def", status="deferred")]))
    )
    assert _queue(board, "undriven_pr")["rows"] == []


def test_a_terminal_node_is_excluded_through_the_closure_helper():
    """A legacy row can carry a stale open `status` beside a real
    `completed_at`; closure is asked of is_terminal_entry, not of status."""
    node = _pr_node("x-done", completed_at="2026-09-01T00:00:00Z")
    board = build_board(_empty_inputs(pr_nodes=_ok([node])))
    assert _queue(board, "undriven_pr")["rows"] == []


def test_a_suspect_claim_is_a_driver_and_never_reaches_undriven_pr():
    """claims.rs treats Suspect like Live for dispatch: TTL-unexpired, holder
    pid unproven, never stolen. Measured 2026-09-03: under load 5 of 15 node
    claims read suspect while their holders' transcripts were seconds old. A
    later widening of _DEAD_CLAIM_STATES must break loudly here."""
    from fno.king.board import _DEAD_CLAIM_STATES

    node = _pr_node("x-susp", pr_number=1400)
    board = build_board(
        _empty_inputs(
            pr_nodes=_ok([node]),
            claims=_ok([_claim("x-susp", state="suspect", holder="target-session:s")]),
        )
    )
    assert _queue(board, "undriven_pr")["rows"] == []
    assert "suspect" not in _DEAD_CLAIM_STATES


def test_an_unreadable_claim_list_builds_no_undriven_rows():
    """A timed-out probe must not read as an absent driver: every node would
    resolve to `none` and the king would dispatch over every live worker at
    once. The loop refuses before a row exists; the render refuses again."""
    node = _pr_node("x-blind")
    board = build_board(
        _empty_inputs(
            pr_nodes=_ok([node]), claims=SourceRead(error="timed out after 5s")
        )
    )
    queue = _queue(board, "undriven_pr")
    assert queue["status"] == "unreadable"
    assert queue["rows"] == []
    assert board["exit_code"] == 1


def test_a_mergeable_green_pr_belongs_to_exactly_one_queue_per_merge_posture():
    """When the king can merge, a green mergeable PR's remedy is the merge
    `mergeable_pr` already names. When it cannot, nobody acts on that queue,
    so the PR still needs a driver and belongs here."""
    node = _pr_node("x-mrg", pr_number=900)
    inputs = _empty_inputs(
        pr_nodes=_ok([node]), prs=_ok([{"number": 900, "title": "t"}])
    )

    mergeable = build_board(inputs, autonomous_merge=True)
    assert _queue(mergeable, "undriven_pr")["rows"] == []

    report_only = build_board(inputs, autonomous_merge=False)
    assert [r["id"] for r in _queue(report_only, "undriven_pr")["rows"]] == ["x-mrg"]


def test_a_p2_node_with_an_open_pr_is_not_undriven_work():
    board = build_board(
        _empty_inputs(pr_nodes=_ok([_pr_node("x-p2", priority="p2")]))
    )
    assert _queue(board, "undriven_pr")["count"] == 0


def test_an_unreadable_pr_nodes_source_makes_the_queue_unreadable_and_actionable():
    """A blind ACTIONABLE queue is work: the king may not exit NoWork while it
    cannot see a queue it could have shrunk."""
    board = build_board(_empty_inputs(pr_nodes=SourceRead(error="graph unreadable")))
    q = _queue(board, "undriven_pr")
    assert q["status"] == "unreadable"
    assert q["actionable"] is True
    assert board["actionable"] >= 1
    assert board["exit_code"] == 1


def test_the_undriven_pr_queue_names_its_verb_and_its_report_only_bound():
    board = build_board(_empty_inputs(pr_nodes=_ok([_pr_node("x-verb")])))
    q = _queue(board, "undriven_pr")
    assert q["verb"] == "/fno:target"
    assert q["actionable"] is True
    assert "never close" in q["note"]
    assert "defer" in q["note"]


def test_a_failing_pr_listing_blinds_both_pr_sources_with_the_same_cause(monkeypatch):
    from fno.king import board as board_mod

    dead = SourceRead(error="exit 1: gh auth expired")
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: dead)

    prs, pr_nodes, warnings = board_mod._read_prs(timeout=1, max_pr_reads=10)

    assert not prs.ok
    assert not pr_nodes.ok
    assert prs.error == pr_nodes.error
    assert warnings == []


# --- truncation -------------------------------------------------------------


def test_the_payload_carries_every_row_so_the_loop_is_never_blind():
    """A silent cap reads as full coverage; the failure-modes section names this.

    This used to assert the OPPOSITE: that the payload itself was capped. That
    cap was the bug. The loop derives each row's identity from this payload to
    tell progress from a stall, so rows dropped here were rows it could never
    see leave. A king draining a long queue cleared row 30, nothing changed in
    the capped list, and it burned to NoProgress while working correctly.

    The cap now lives only in the renderer, where a human is the consumer.
    """
    prs = [{"number": n, "title": "t"} for n in range(50)]
    board = build_board(_empty_inputs(prs=_ok(prs)), autonomous_merge=True)

    q = _queue(board, "mergeable_pr")
    assert q["count"] == 50
    assert len(q["rows"]) == 50, "the machine consumer must see every row"
    assert "truncated" not in q, (
        "a truncation field on the payload invites a reader to trust a short list"
    )


def test_the_renderer_elides_and_says_so(capsys):
    """The cap is legitimate for a human, as long as it announces itself."""
    from fno.king.cli import _render

    prs = [{"number": n, "title": "t"} for n in range(50)]
    board = build_board(_empty_inputs(prs=_ok(prs)), autonomous_merge=True)

    _render(board, 10)

    out = capsys.readouterr().out
    assert "... 40 more not shown" in out, out


# --- BREAK 2/3: the board reports every stream the outstanding verb returns ---


def _outstanding(
    *,
    questions=(),
    carveouts=None,
    captures=None,
    roots=None,
):
    """The whole `fno inbox outstanding --json` payload, defaulting to empty."""
    return _ok(
        {
            "questions": list(questions),
            "carveouts": carveouts or {"total": 0, "by_kind": {}, "oldest_ts": None},
            "captures": captures or {"total": 0, "by_project": {}},
            "roots": roots or {},
        }
    )


def test_the_board_reports_carveouts_and_captures_not_questions_alone():
    """The board kept one of four streams; 661 of 665 items were invisible."""
    inputs = _empty_inputs(
        outstanding=_outstanding(
            questions=[{"id": "q-1", "question": "which lane?"}],
            carveouts={
                "total": 14,
                "by_kind": {"oos-bug": 11, "deferred": 3},
                "oldest_ts": None,
            },
            captures={"total": 647, "by_project": {"c3po": 400, "footnote": 247}},
            roots={"carveouts": {"scope": "project", "root": "/repos/footnote"}},
        )
    )
    board = build_board(inputs)

    cq = _queue(board, "carveout_pending")
    assert cq["count"] == 14
    assert cq["actionable"] is False, "a human-gated stream never gates NoWork"
    assert {"kind": "oos-bug", "n": 11} in cq["rows"]
    assert "/repos/footnote" in cq["note"], "the queue names the root its count came from"

    capq = _queue(board, "capture_pending")
    assert capq["count"] == 647
    assert capq["actionable"] is False
    assert {"project": "footnote", "n": 247} in capq["rows"]

    assert _queue(board, "operator_question")["count"] == 1
    assert board["actionable"] == 0


def test_capture_projects_beyond_the_row_cap_state_the_elision():
    """A hundreds-row stream cannot be listed; the cut must be said out loud."""
    projects = {f"p{i:02d}": i + 1 for i in range(12)}
    inputs = _empty_inputs(
        outstanding=_outstanding(captures={"total": 78, "by_project": projects})
    )
    board = build_board(inputs)

    q = _queue(board, "capture_pending")
    listed = [r for r in q["rows"] if "project" in r]
    assert len(listed) == 8
    assert any(r.get("elided_projects") == 4 for r in q["rows"]), (
        "the elision row names how many projects were cut"
    )


def test_all_four_streams_are_visible_from_one_board_read():
    inputs = _empty_inputs(
        outstanding=_outstanding(
            carveouts={"total": 2, "by_kind": {"oos-bug": 2}, "oldest_ts": None},
            captures={"total": 3, "by_project": {"vault": 3}},
        ),
        lane=_ok([_lane_item("ship the thing")]),
    )
    board = build_board(inputs)

    assert _queue(board, "operator_lane")["count"] == 1
    assert _queue(board, "operator_question")["count"] == 0
    assert _queue(board, "carveout_pending")["count"] == 2
    assert _queue(board, "capture_pending")["count"] == 3


def test_a_nested_shape_change_degrades_that_stream_not_the_whole_board():
    """The board shells out through a PATH-resolved fno; a stale deployed CLI
    can answer an older stream shape. The promise is degrade, never crash."""
    inputs = _empty_inputs(
        outstanding=_ok(
            {
                "questions": [],
                "carveouts": ["not", "a", "dict"],
                "captures": {"total": "many", "by_project": {"a": "x", "b": 2}},
                "roots": None,
            }
        )
    )
    board = build_board(inputs)

    assert _queue(board, "carveout_pending")["count"] == 0
    cap = _queue(board, "capture_pending")
    assert cap["count"] == 0, '"many" is not a count; zero, not a crash'
    assert {"project": "b", "n": 2} in cap["rows"]
    assert {"project": "a", "n": 0} in cap["rows"], "junk counts render as 0"


# --- the claimed-nodes read: one graph read, zero per-claim subprocesses ----
# x-3761: the per-claim `fno backlog get` loop cost 30.8s for 13 claims on an
# idle machine against this board's own 30s stop-gate ceiling, so every king
# fire died on `king board unreadable` before reaching a decision. The lookup
# reads the graph in process now; these tests pin that shape.


def _graph_file(tmp_path, entries):
    graph_path = tmp_path / "graph.json"
    graph_path.write_text(json.dumps({"entries": entries}))
    return graph_path


def _isolate_graph(monkeypatch, tmp_path, entries):
    from fno import paths

    monkeypatch.setattr(paths, "graph_json", lambda: _graph_file(tmp_path, entries))
    monkeypatch.setattr(paths, "graph_archive_json", lambda: tmp_path / "no-archive.json")


def test_a_live_claim_resolves_without_a_single_subprocess(monkeypatch, tmp_path):
    """Zero spawns is the regression guard; the resolved row is the positive
    control. Zero spawns with zero rows would prove the fixture never got
    read, not that the read got cheap."""
    from fno.king import board as board_mod

    _isolate_graph(
        monkeypatch,
        tmp_path,
        [{"id": "x-1111", "status": "in_progress", "priority": "p0", "title": "held work"}],
    )
    spawns = []
    real_run = board_mod.subprocess.run

    def counting_run(cmd, *args, **kwargs):
        spawns.append(cmd)
        return real_run(cmd, *args, **kwargs)

    monkeypatch.setattr(board_mod.subprocess, "run", counting_run)

    nodes, holders, warnings = board_mod._read_claimed_nodes(_ok([_claim("x-1111")]))

    assert [n["id"] for n in nodes.rows()] == ["x-1111"], "the claim must resolve to its node row"
    assert holders == {"target-session:abc"}, "a p0 holder feeds the activity read"
    assert spawns == [], "a claimed-node lookup must never spawn a subprocess"
    assert warnings == []


def test_a_claim_on_an_absent_node_warns_and_the_queue_stays_readable(monkeypatch, tmp_path):
    """One missing node degrades to a warning exactly as a failed per-claim
    read did; it must never turn the stalled_holder source unreadable."""
    from fno.king import board as board_mod

    _isolate_graph(
        monkeypatch, tmp_path, [{"id": "x-1111", "status": "ready", "priority": "p1"}]
    )

    nodes, holders, warnings = board_mod._read_claimed_nodes(
        _ok([_claim("x-9999"), _claim("x-1111", holder="target-session:def")])
    )

    assert nodes.ok, "a missing node is not an unreadable queue"
    assert [n["id"] for n in nodes.rows()] == ["x-1111"]
    assert holders == {"target-session:def"}
    assert len(warnings) == 1 and "x-9999" in warnings[0]


def test_claims_beyond_the_old_20_read_cap_all_resolve(monkeypatch, tmp_path):
    """The 20-claim cap existed because each lookup was a subprocess. Reads
    are one file read now, so a cap would only destroy coverage."""
    from fno.king import board as board_mod

    ids = [f"cap-{i:04d}" for i in range(25)]
    _isolate_graph(
        monkeypatch, tmp_path, [{"id": i, "status": "in_progress", "priority": "p1"} for i in ids]
    )

    nodes, holders, warnings = board_mod._read_claimed_nodes(
        _ok([_claim(i, holder=f"target-session:{i}") for i in ids])
    )

    assert len(nodes.rows()) == 25
    assert len(holders) == 25
    assert not any("capped" in w for w in warnings)


def test_a_terminal_nodes_claim_drops_at_source(monkeypatch, tmp_path):
    """A done node's leftover claim is a leak for the reaper, never king work;
    dropping it at the source also keeps its holder out of the transcript
    reads."""
    from fno.king import board as board_mod

    _isolate_graph(
        monkeypatch,
        tmp_path,
        [
            {"id": "x-done", "status": "done", "priority": "p0"},
            {"id": "x-live", "status": "in_progress", "priority": "p0"},
        ],
    )

    nodes, holders, warnings = board_mod._read_claimed_nodes(
        _ok([_claim("x-done"), _claim("x-live", holder="target-session:def")])
    )

    assert [n["id"] for n in nodes.rows()] == ["x-live"]
    assert holders == {"target-session:def"}
    assert warnings == []


@pytest.mark.e2e
def test_the_live_board_read_fits_the_stop_gate_ceiling_by_duration():
    """x-3761's acceptance is SECONDS, not rows: the read returned 123 rows
    and still died at the 30s stop-gate ceiling. Opt-in (`FNO_KING_BOARD_LIVE=1`)
    because the claim needs the operator's real graph, claims and open PRs -
    the hermetic suite sandboxes HOME, so a default run has no world to time.
    This runs the gate's own command (`inbox board --json`, the read
    `bounded_read` kills at STOPGATE_READ_TIMEOUT) as a subprocess against the
    real HOME so it pays the same interpreter and import cost a stop fire pays,
    times it, and asserts the number. The parse-shape assertions are the "stop
    hook reaches its decision" check: `parse_king_board` needs an integer
    `actionable` and a `queues` array, or the fire renders `king board
    unreadable` no matter how fast the read was.
    """
    import os
    import subprocess
    import sys
    import time

    if not os.environ.get("FNO_KING_BOARD_LIVE"):
        pytest.skip(
            "the duration claim needs the real board; set FNO_KING_BOARD_LIVE=1 to run it"
        )
    real_home = next(
        (
            candidate
            for candidate in (
                os.path.join("/Users", os.environ.get("USER", "")),
                os.path.join("/home", os.environ.get("USER", "")),
                "/root",
            )
            if os.path.isdir(candidate)
        ),
        None,
    )
    assert real_home is not None, "the live board read could not locate the real HOME"

    env = {**os.environ, "HOME": real_home, "USERPROFILE": real_home, "NO_COLOR": "1"}
    # The hermetic suite flags its children FNO_TEST_HERMETIC=1 so a journal
    # write cannot reach the live events.jsonl. This child IS the deliberate
    # real-world read, so the flag must not travel with it.
    env.pop("FNO_TEST_HERMETIC", None)
    start = time.monotonic()
    proc = subprocess.run(
        [sys.executable, "-m", "fno.cli", "inbox", "board", "--json"],
        capture_output=True,
        text=True,
        env=env,
        timeout=300,
    )
    elapsed = time.monotonic() - start
    assert elapsed < 30.0, f"board read took {elapsed:.1f}s against the 30.0s stop-gate ceiling"
    # The board exits 1 when a queue is unreadable and STILL prints the full
    # payload; the gate parses that payload regardless of the exit code, so
    # only an absent one is a decision-blocking failure.
    assert proc.stdout.strip(), (
        f"board printed no payload (rc={proc.returncode}); stderr: {proc.stderr.strip()[-400:]}"
    )

    board = json.loads(proc.stdout)
    assert isinstance(board.get("actionable"), int), "parse_king_board cannot decide without it"
    queues = board.get("queues")
    assert isinstance(queues, list) and queues, "parse_king_board needs a queues array"
    for q in queues:
        assert isinstance(q.get("name"), str), f"queue missing a name: {q}"
        assert q.get("status") in {"ok", "unreadable"}, f"queue status undecidable: {q}"
