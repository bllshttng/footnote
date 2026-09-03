"""The king board's budget flows inward and the board always answers (x-f8e3).

Three properties under test:

1. Every per-source slice is DERIVED from the one whole-board total; no source
   can be offered more than remains, and the total offered is less than the
   budget by the serialization reserve.
2. An independent per-source default does not exist: the hand-run constant
   budgets the whole board when no flag is passed, and nothing else.
3. When the budget is spent the board STOPS, names every unstarted source in
   its own queue error, and returns a parseable payload - it is never killed
   by the caller's timer with nothing to show.
"""

from __future__ import annotations

import json

import pytest

from fno.king import board


def _budget_capture(monkeypatch, budget_ms):
    """Run collect_inputs with every reader stubbed, recording the slice each
    source was offered. Returns ({source_name: slice_s_or_None}, BoardInputs)."""
    slices: dict[str, float | None] = {}

    def fake_undispatched():
        return board.SourceRead(payload=[])

    def fake_claims():
        return board.SourceRead(payload=[])

    def fake_prs(timeout_s, max_pr_reads):
        slices["gh pr list"] = timeout_s
        return board.SourceRead(payload=[]), []

    def fake_claimed(claims):
        slices["stalled_holder lookups"] = 1.0
        return board.SourceRead(payload=[]), set(), []

    def fake_outstanding():
        return board.SourceRead(payload={})

    def fake_run_json(cmd, *, timeout):
        slices[" ".join(cmd[:2])] = timeout
        return board.SourceRead(payload=[])

    def fake_holder_activity(holders):
        return {}

    monkeypatch.setattr(board, "_read_undispatched", fake_undispatched)
    monkeypatch.setattr(board, "_read_claims", fake_claims)
    monkeypatch.setattr(board, "_read_prs", fake_prs)
    monkeypatch.setattr(board, "_read_claimed_nodes", fake_claimed)
    monkeypatch.setattr(board, "_read_outstanding", fake_outstanding)
    monkeypatch.setattr(board, "_run_json", fake_run_json)
    monkeypatch.setattr(board, "_resolve_holder_activity", fake_holder_activity)

    inputs = board.collect_inputs(budget_ms=budget_ms)
    return slices, inputs


def test_no_source_slice_exceeds_the_remaining_total(monkeypatch):
    slices, _ = _budget_capture(monkeypatch, 30_000)
    assert slices, "every source must have been offered a slice"
    for name, offered in slices.items():
        assert offered is not None, f"{name} was marked not-read on a generous budget"
        assert offered <= 30.0, f"{name} was offered more than the whole budget"


def test_derived_slices_stay_inside_the_budget_minus_reserve(monkeypatch):
    # Each slice is derived from what REMAINS, minus the reserve, so no slice
    # may exceed (budget - reserve) and the slices shrink as the budget is
    # consumed. The reserve is untouchable by any source.
    slices, _ = _budget_capture(monkeypatch, 30_000)
    ceiling = 30.0 - board.SERIALIZE_RESERVE_MS / 1000.0
    for name, offered in slices.items():
        assert offered is not None
        assert offered <= ceiling, f"{name} was offered the reserve itself"


def test_hand_run_budget_is_the_whole_board(monkeypatch):
    # AC1-ERR: flag absent -> the hand-run constant applies and no source is
    # offered more than it. The constant budgets the BOARD, not a source.
    monkeypatch.delenv("FNO_KING_BOARD_TIMEOUT", raising=False)
    assert board._hand_run_budget_ms() == board.HAND_RUN_BUDGET_MS
    slices, _ = _budget_capture(monkeypatch, board.HAND_RUN_BUDGET_MS)
    for name, offered in slices.items():
        assert offered is not None
        assert offered <= board.HAND_RUN_BUDGET_MS / 1000.0


def test_env_override_degrades_loudly_not_fatal(monkeypatch, capsys):
    # A typo'd env var must not read as a broken board (the refusal that used
    # to reach the Rust caller as "output unparseable").
    monkeypatch.setattr("os.environ", {"FNO_KING_BOARD_TIMEOUT": "abc"})
    assert board._hand_run_budget_ms() == board.HAND_RUN_BUDGET_MS
    assert "not an integer" in capsys.readouterr().err


def test_exhausted_budget_names_every_unstarted_source(monkeypatch):
    # AC1-EDGE + AC2-HP: a budget smaller than one source's minimum -> the
    # board returns IMMEDIATELY, every source named unread with the budget
    # marker, exit code 1, and a payload the Rust caller can parse.
    tiny = board.SERIALIZE_RESERVE_MS + 1  # nothing left after the reserve
    inputs = board.collect_inputs(budget_ms=tiny)
    payload = board.build_board(inputs)
    assert payload["exit_code"] == 1
    assert payload["unreadable"] > 0
    named = [q for q in payload["queues"] if "budget exhausted" in (q.get("error") or "")]
    assert named, "unstarted sources must be named, never silently absent"
    for q in named:
        assert q["status"] == "unreadable"


def test_budget_exhaustion_marker_names_the_last_source_started(monkeypatch):
    # The marker names WHAT the budget was exhausted after, so the king's
    # refusal quotes a source, never an elapsed time (x-1595's rule).
    # One millisecond over the reserve: the first source starts (its slice is
    # that millisecond), everything after it is named.
    tiny = board.SERIALIZE_RESERVE_MS + 1

    def slow_first():
        import time

        time.sleep(0.05)
        return board.SourceRead(payload=[])

    monkeypatch.setattr(board, "_read_undispatched", slow_first)
    inputs = board.collect_inputs(budget_ms=tiny)
    payload = board.build_board(inputs)
    errors = [q["error"] for q in payload["queues"] if q.get("error")]
    marked = [e for e in errors if "budget exhausted" in e]
    assert marked
    assert any("exhausted after backlog undispatched" in e for e in marked)


def test_one_raising_in_process_source_degrades_alone(monkeypatch):
    # AC3-ERR: an in-process source raising lands as a SourceRead error - the
    # same shape a non-zero exit produces today - and every other queue still
    # answers.
    def raising():
        raise RuntimeError("store exploded")

    monkeypatch.setattr(board, "_read_undispatched", raising)
    monkeypatch.setattr(board, "_read_claims", lambda: board.SourceRead(payload=[]))
    monkeypatch.setattr(board, "_read_prs", lambda t, m: (board.SourceRead(payload=[]), []))
    monkeypatch.setattr(board, "_read_claimed_nodes", lambda c: (board.SourceRead(payload=[]), set(), []))
    monkeypatch.setattr(board, "_read_outstanding", lambda: board.SourceRead(payload={}))
    monkeypatch.setattr(board, "_run_json", lambda cmd, *, timeout: board.SourceRead(payload=[]))
    monkeypatch.setattr(board, "_resolve_holder_activity", lambda holders: {})

    payload = board.build_board(board.collect_inputs(budget_ms=30_000))
    bad = [q for q in payload["queues"] if q["status"] == "unreadable"]
    assert [q["name"] for q in bad] == ["undispatched"]
    assert "store exploded" in bad[0]["error"]
    by_name = {q["name"]: q for q in payload["queues"]}
    assert by_name["unplanned"]["status"] == "ok"
    assert by_name["mergeable_pr"]["status"] == "ok"


def test_derived_slices_are_never_negative(monkeypatch):
    # A budget below the reserve must not offer a negative timeout to any
    # subprocess - the sources are simply named not-read.
    slices, inputs = _budget_capture(monkeypatch, 500)
    for name, offered in slices.items():
        assert offered is None, f"{name} was offered a slice from a spent budget"
    payload = board.build_board(inputs)
    assert json.dumps(payload)  # payload serializes
    assert payload["exit_code"] == 1
