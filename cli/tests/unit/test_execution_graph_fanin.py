"""Fan-in completeness: tally_fan_in + orchestrator.classify_worker_return.

Proves the deterministic barrier count refuses synthesis on any unresolved
input. Feeds null, empty, malformed, duplicate, failed, and missing worker
returns end-to-end (raw output string -> classify -> tally) and asserts exact
expected-versus-observed counts, per the plan's verification step 4.
"""
from __future__ import annotations

import importlib.util
from pathlib import Path

from fno.events.verify_child_promise import FanInTally, tally_fan_in

REPO = Path(__file__).resolve().parents[3]


def _load_orch():
    spec = importlib.util.spec_from_file_location(
        "do_orchestrator", REPO / "skills/execute/orchestrator.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _classify_many(outputs):
    orch = _load_orch()
    return [orch.classify_worker_return(o) for o in outputs]


# ── tally directly ───────────────────────────────────────────────────────────


def test_all_expected_completed_is_complete():
    t = tally_fan_in(["a", "b"], [("a", "completed"), ("b", "completed")])
    assert t == FanInTally(expected=2, completed=2, failed=0, duplicate=0, malformed=0, missing=0)
    assert t.complete is True


def test_empty_observations_leave_everything_missing():
    t = tally_fan_in(["a", "b"], [])
    assert t.missing == 2 and t.completed == 0
    assert t.complete is False


def test_missing_one_blocks():
    t = tally_fan_in(["a", "b"], [("a", "completed")])
    assert t.missing == 1
    assert t.complete is False


def test_failed_blocks_even_when_all_present():
    t = tally_fan_in(["a", "b"], [("a", "completed"), ("b", "failed")])
    assert t.failed == 1 and t.missing == 0
    assert t.complete is False


def test_malformed_counted_and_blocks():
    # None id and unknown kind are both malformed, attributed to no node.
    t = tally_fan_in(["a"], [(None, "completed"), ("a", "garbage")])
    assert t.malformed == 2 and t.completed == 0 and t.missing == 1
    assert t.complete is False


def test_duplicate_counted_first_wins():
    t = tally_fan_in(["a"], [("a", "completed"), ("a", "failed")])
    assert t.duplicate == 1 and t.completed == 1 and t.failed == 0
    assert t.complete is True  # first observation for a wins; a completed


def test_expected_set_deduplicated():
    t = tally_fan_in(["a", "a", "b"], [("a", "completed"), ("b", "completed")])
    assert t.expected == 2
    assert t.complete is True


def test_unexpected_id_is_ignored_not_counted():
    # a return for an id outside the expected set must not skew the tally
    t = tally_fan_in(["a"], [("a", "completed"), ("z", "completed")])
    assert t == FanInTally(expected=1, completed=1, failed=0, duplicate=0, malformed=0, missing=0)
    assert t.complete is True


def test_fanintally_rejects_negative_fields():
    import pytest

    with pytest.raises(ValueError):
        FanInTally(expected=-1, completed=0, failed=0, duplicate=0, malformed=0, missing=0)


def test_to_dict_includes_complete_flag():
    d = tally_fan_in(["a"], [("a", "completed")]).to_dict()
    assert d["complete"] is True and d["expected"] == 1


# ── end-to-end: raw worker output -> classify -> tally ───────────────────────


def test_classify_maps_canonical_statuses():
    outs = [
        '```json\n{"result":"SUCCESS","task":"a"}\n```',
        '```json\n{"result":"DONE_WITH_CONCERNS","task":"b"}\n```',
        '```json\n{"result":"FAILED","task":"c","error":"boom"}\n```',
        '```json\n{"result":"BLOCKED","task":"d","reason":"dep"}\n```',
    ]
    pairs = _classify_many(outs)
    assert pairs == [("a", "completed"), ("b", "completed"), ("c", "failed"), ("d", "failed")]


def test_null_and_empty_output_are_malformed():
    pairs = _classify_many(["", "no contract here", '{"result":"NONSENSE","task":"x"}'])
    assert pairs == [(None, None), (None, None), (None, None)]


def test_end_to_end_mixed_bag_blocks_synthesis():
    outputs = [
        '```json\n{"result":"SUCCESS","task":"a"}\n```',   # completed
        '```json\n{"result":"SUCCESS","task":"a"}\n```',   # duplicate
        '```json\n{"result":"FAILED","task":"b"}\n```',    # failed
        "garbage output",                                     # malformed
        # expected id "c" never returns                       -> missing
    ]
    observed = _classify_many(outputs)
    t = tally_fan_in(["a", "b", "c"], observed)
    assert t.expected == 3
    assert t.completed == 1
    assert t.failed == 1
    assert t.duplicate == 1
    assert t.malformed == 1
    assert t.missing == 1
    assert t.complete is False


def test_end_to_end_all_good_completes():
    outputs = [
        '```json\n{"result":"SUCCESS","task":"a"}\n```',
        '```json\n{"result":"DONE_WITH_CONCERNS","task":"b"}\n```',
    ]
    t = tally_fan_in(["a", "b"], _classify_many(outputs))
    assert t.complete is True
