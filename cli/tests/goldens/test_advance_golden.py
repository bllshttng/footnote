"""Golden receipts: advance (the dispatch orchestrator's door shapes).

The explain report embeds live gate measurements (fleet rows, claim states),
so only its stable skeleton lines are pinned.
"""
from __future__ import annotations

from tests.goldens._door import door, make_sandbox, seed_node, warm


def test_advance_without_auto_continue_skips_itself(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ddd44000", "ready"), seed_node("x-eee55000", "done")])
    warm(root, "x-ddd44000")
    code, out, err = door(root, ["advance"])
    assert code == 0, err
    assert out == "skipped reason=disabled\n", out


def test_advance_explain_reports_the_selection_grid_and_declines_to_act(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-ddd44000", "ready"), seed_node("x-eee55000", "done")])
    warm(root, "x-ddd44000")
    code, out, err = door(root, ["advance", "--explain"])
    assert code == 0, err
    assert "SELECTION  1 candidates -> 1 eligible\n" in out
    assert "-> 1. x-ddd44000  p2" in out
    assert "GATES\n" in out
    assert "would dispatch: x-ddd44000\n" in out
    assert "advance is DISARMED, so nothing above would run automatically." in out
    assert "This report is a dry run of the pipeline, not a record of a decision advance made." in out


def test_advance_explain_with_no_candidates_declines_to_route(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-eee55000", "done")])
    warm(root, "x-eee55000")
    code, out, err = door(root, ["advance", "--explain"])
    assert code == 0, err
    assert "SELECTION  0 candidates -> 0 eligible\n" in out
    assert "(no chain: nothing to route)\n" in out
    assert "would dispatch: nothing (no eligible node)\n" in out
