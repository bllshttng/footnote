"""Pure-fold + isolation coverage for the observer harness (x-57a5).

The fold's own ``__main__`` self-check covers the load-bearing invariants; these
add pytest coverage the plan's Task 7 asks for: attribution/outcome join, the
<10 insufficient guard, coverage_pct honesty, structural checks, review
precision (pass/degraded/fail), the replay None-dims path (A1), postmortem
corrupt-line tolerance, and the isolation-violation detective scan.
"""

from __future__ import annotations

from datetime import datetime

from fno.observer import fold, isolation

NOW = datetime(2026, 7, 4, 12, 0, 0)


def _blueprint_row(node_id, session_id, completed="2026-07-01T10:00:00", reason="DonePRGreen"):
    return {
        "completed": completed,
        "termination_reason": reason,
        "graph_node_id": node_id,
        "sessions": [session_id],
        "phases_completed": ["plan"],  # -> attributed to fno:blueprint
    }


def test_build_corpus_attribution_and_outcome_join():
    rows = [
        _blueprint_row("x-1", "s1"),
        {**_blueprint_row("x-2", "s2"), "phases_completed": ["do"]},  # not blueprint
    ]
    nodes = [{"id": "x-1", "reverted": False}, {"id": "x-2", "reverted": False}]
    corpus = fold.build_corpus(rows, nodes, [], skill="blueprint", since_days=28, now=NOW)
    assert corpus["total_rows"] == 2
    assert corpus["attributed"] == 1
    assert corpus["items"][0]["outcome"] == "merged_clean"
    assert corpus["items"][0]["session_id"] == "s1"


def test_insufficient_guard_below_ten():
    summary = fold.build_run_summary(
        run_id="obs-x", skill_id="fno:blueprint", skill_version="unknown",
        findings=[("structural_validity", "pass")], corpus_size=3, scored_count=3,
    )
    assert summary == {"state": "insufficient", "need": 10, "n": 3}


def test_coverage_pct_rounds_scored_over_corpus():
    summary = fold.build_run_summary(
        run_id="obs-x", skill_id="fno:blueprint", skill_version="abc1234",
        findings=[("structural_validity", "pass")], corpus_size=12, scored_count=10,
    )
    assert summary["state"] == "ok"
    assert summary["coverage_pct"] == 83  # round(100*10/12)


def test_structural_checks():
    assert fold.has_failure_modes_heading("# P\n\n## Failure Modes\n\nx\n") is True
    assert fold.has_failure_modes_heading("# P\n\n## Overview\nx\n") is False
    strat = (
        "## Execution Strategy\n\n```yaml\n"
        "tasks:\n- id: '1'\n  surface: ['a.py','b.py']\n"
        "- id: '2'\n  surface: ['b.py','c.py']\n```\n"
    )
    assert fold.find_file_ownership_collisions(strat) == ["b.py"]


def test_review_precision_pass_degraded_fail():
    assert fold.score_review_item(addressed_ids={"c1"}, skipped_ids={"c2"}, all_finding_ids={"c1", "c2"}) == {"finding_precision": "pass"}
    assert fold.score_review_item(addressed_ids={"c1"}, skipped_ids=set(), all_finding_ids={"c1", "c2"}) == {"finding_precision": "degraded"}
    assert fold.score_review_item(addressed_ids=set(), skipped_ids=set(), all_finding_ids={"c1", "c2"}) == {"finding_precision": "fail"}
    # no findings at all -> not scorable, a coverage gap
    assert fold.score_review_item(addressed_ids=set(), skipped_ids=set(), all_finding_ids=set()) == {"finding_precision": None}


def test_replay_path_scores_structural_only_and_none_without_plan():
    # A1: replay item omits shipped_outcome; plan_text=None -> structural None.
    item = {"include_shipped_outcome": False}
    scored = fold.score_blueprint_item(item, plan_text=None)
    assert scored == {"structural_validity": None, "collision_free": None}
    assert "shipped_outcome" not in scored
    # with fresh output, structural dims resolve (no shipped_outcome ever)
    scored2 = fold.score_blueprint_item(item, plan_text="## Failure Modes\nx\n")
    assert scored2["structural_validity"] == "pass"
    assert "shipped_outcome" not in scored2


def test_postmortem_reader_tolerates_malformed(tmp_path):
    from fno.observer.cli import _read_postmortems

    (tmp_path / "good.md").write_text(
        "---\nsession_id: s1\ngraph_node_id: x-1\nblocked_reason:\n  kind: missing_dependency\n---\nbody\n"
    )
    (tmp_path / "bad.md").write_text("---\n: : not: valid: yaml:\n---\nbody\n")
    (tmp_path / "no-fm.md").write_text("just text, no frontmatter\n")
    out = _read_postmortems(tmp_path)
    kinds = {p["blocked_reason_kind"] for p in out}
    assert "missing_dependency" in kinds  # the good one survived
    # malformed / no-frontmatter never crash the read
    assert all(isinstance(p, dict) for p in out)


def test_isolation_violation_detected(tmp_path):
    leaked = "20260704T999999Z-leak"
    real_ledger = tmp_path / "ledger.json"
    real_ledger.write_text('{"entries":[{"session_id":"' + leaked + '","cost_usd":1}]}\n')
    result = isolation.check_isolation({leaked}, {"ledger_json": real_ledger})
    assert result.verdict == "violated"
    assert result.violations[0].session_id == leaked
    # a clean scan (id absent) -> clean
    assert isolation.check_isolation({"not-present"}, {"ledger_json": real_ledger}).verdict == "clean"
