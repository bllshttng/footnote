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
        questions=_ok([]),
        needs=_ok([]),
    )
    base.update(overrides)
    return BoardInputs(**base)


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
    read, _ = board_mod._read_prs(timeout=1, max_pr_reads=10)
    assert len(read.rows()) == expected


def test_an_unmergeable_pr_is_not_the_kings_work(monkeypatch):
    from fno.king import board as board_mod

    listing = [
        {"number": 1, "title": "t", "mergeable": "CONFLICTING", "statusCheckRollup": []}
    ]
    monkeypatch.setattr(board_mod, "_run_json", lambda *a, **k: _ok(listing))
    read, _ = board_mod._read_prs(timeout=1, max_pr_reads=10)
    assert read.rows() == []


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
    _, warnings = board_mod._read_prs(timeout=1, max_pr_reads=5)
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
    read, _ = board_mod._read_prs(timeout=1, max_pr_reads=5)
    assert len(read.rows()) == 8


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
