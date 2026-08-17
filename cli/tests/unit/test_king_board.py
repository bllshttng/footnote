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
        holder_activity={},
        prs=_ok([]),
        questions=_ok([]),
        needs=_ok([]),
    )
    base.update(overrides)
    return BoardInputs(**base)


def _node(node_id, priority="p0", plan_path="/plans/p.md"):
    return {"id": node_id, "priority": priority, "plan_path": plan_path}


def _claim(node_id, state="live", holder="target-session:abc"):
    return {"key": f"node:{node_id}", "state": state, "holder": holder}


def _queue(board, name):
    for q in board["queues"]:
        if q["name"] == name:
            return q
    raise AssertionError(f"no queue named {name}: {[q['name'] for q in board['queues']]}")


# --- AC1-HP -----------------------------------------------------------------


def test_undispatched_row_names_the_node_and_carries_a_rerunnable_source():
    board = build_board(_empty_inputs(ready=_ok([_node("x-1234")])))

    q = _queue(board, "undispatched")
    assert q["actionable"] is True
    assert q["count"] == 1
    assert [r["id"] for r in q["rows"]] == ["x-1234"]
    assert q["source"].startswith("fno backlog ready")
    assert board["actionable"] == 1


def test_every_queue_carries_a_source_command():
    board = build_board(_empty_inputs())
    assert len(board["queues"]) == 6
    for q in board["queues"]:
        assert q["source"], f"{q['name']} has no source command"


def test_p2_node_is_not_undispatched_work():
    board = build_board(_empty_inputs(ready=_ok([_node("x-1234", priority="p2")])))
    assert _queue(board, "undispatched")["count"] == 0
    assert board["actionable"] == 0


def test_node_without_a_plan_is_not_undispatched_work():
    board = build_board(_empty_inputs(ready=_ok([_node("x-1234", plan_path=None)])))
    assert _queue(board, "undispatched")["count"] == 0


def test_a_claim_of_any_state_takes_a_node_out_of_undispatched():
    inputs = _empty_inputs(
        ready=_ok([_node("x-1234")]),
        claims=_ok([_claim("x-1234", state="stale")]),
    )
    board = build_board(inputs)
    assert _queue(board, "undispatched")["count"] == 0
    assert _queue(board, "stale_claim")["count"] == 1


# --- AC1-EDGE ---------------------------------------------------------------


def test_human_gated_queues_report_their_count_without_gating_nowork():
    inputs = _empty_inputs(
        questions=_ok([{"id": "q-1", "question": "which lane?"}]),
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


def test_node_held_by_an_inactive_holder_is_stalled_not_undispatched():
    inputs = _empty_inputs(
        ready=_ok([_node("x-1234")]),
        claims=_ok([_claim("x-1234", holder="target-session:dead")]),
        holder_activity={"target-session:dead": {"state": "stalled", "age_s": 9000}},
    )
    board = build_board(inputs)

    assert [r["id"] for r in _queue(board, "stalled_holder")["rows"]] == ["x-1234"]
    assert _queue(board, "undispatched")["count"] == 0
    assert board["actionable"] == 1


def test_node_held_by_an_active_holder_appears_in_neither_queue():
    inputs = _empty_inputs(
        ready=_ok([_node("x-1234")]),
        claims=_ok([_claim("x-1234", holder="target-session:busy")]),
        holder_activity={"target-session:busy": {"state": "working", "age_s": 12}},
    )
    board = build_board(inputs)

    assert _queue(board, "stalled_holder")["count"] == 0
    assert _queue(board, "undispatched")["count"] == 0
    assert board["actionable"] == 0


def test_an_active_state_with_a_stale_age_is_still_stalled():
    """`working` with a two-hour-old transcript is the wedged worker; the age is
    what makes the activity axis actionable rather than decorative."""
    inputs = _empty_inputs(
        ready=_ok([_node("x-1234")]),
        claims=_ok([_claim("x-1234", holder="target-session:wedged")]),
        holder_activity={"target-session:wedged": {"state": "working", "age_s": 60 * 60 * 9}},
    )
    board = build_board(inputs)
    assert _queue(board, "stalled_holder")["count"] == 1


def test_unresolvable_holder_activity_is_stalled_not_staffed():
    inputs = _empty_inputs(
        ready=_ok([_node("x-1234")]),
        claims=_ok([_claim("x-1234", holder="target-session:ghost")]),
        holder_activity={},
    )
    board = build_board(inputs)
    assert _queue(board, "stalled_holder")["count"] == 1


# --- AC1c-ERR ---------------------------------------------------------------


def test_a_holder_whose_wire_status_reads_live_is_still_stalled_when_parked():
    """The specific regression this file exists to catch.

    `WIRE_STATUS` renders working, parked and model-refused all as `live`, so a
    board that asked `status == "live"` would call this lane staffed. The
    fixture pins `status` to exactly `live` so the wrong read fails loudly.
    """
    inputs = _empty_inputs(
        ready=_ok([_node("x-1234")]),
        claims=_ok([_claim("x-1234", holder="target-session:parked")]),
        holder_activity={
            "target-session:parked": {
                "state": "done",
                "age_s": 30,
                "status": "live",
                "reachability": "reachable",
            }
        },
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
    inputs = _empty_inputs(questions=SourceRead(error="timed out after 60s"))
    board = build_board(inputs)

    assert _queue(board, "operator_question")["status"] == "unreadable"
    assert board["exit_code"] != 0
    assert board["actionable"] == 0


def test_a_healthy_board_exits_zero_and_is_json_serialisable():
    board = build_board(_empty_inputs())
    assert board["actionable"] == 0
    assert board["exit_code"] == 0
    json.dumps(board)


# --- truncation -------------------------------------------------------------


def test_a_capped_queue_reports_its_truncation():
    """A silent cap reads as full coverage; the failure-modes section names this."""
    prs = [{"number": n, "title": "t"} for n in range(50)]
    board = build_board(_empty_inputs(prs=_ok(prs)), autonomous_merge=True, max_rows=10)

    q = _queue(board, "mergeable_pr")
    assert len(q["rows"]) == 10
    assert q["count"] == 50
    assert q["truncated"] == 40
