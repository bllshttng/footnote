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


def test_replay_batch_skips_min_gate_and_reports_true_size():
    # require_min=False (a replay is a targeted before/after, not a >=10 trend):
    # a single-item batch reports corpus_size=1, never a padded 10.
    summary = fold.build_run_summary(
        run_id="obs-x", skill_id="fno:blueprint", skill_version="unknown",
        findings=[("structural_validity", "pass")], corpus_size=1, scored_count=1,
        skill_ref=None, require_min=False,
    )
    assert summary["state"] == "ok"
    assert summary["corpus_size"] == 1
    assert summary["coverage_pct"] == 100
    # the default (sweep) still gates
    gated = fold.build_run_summary(
        run_id="obs-x", skill_id="fno:blueprint", skill_version="unknown",
        findings=[("structural_validity", "pass")], corpus_size=1, scored_count=1,
    )
    assert gated["state"] == "insufficient"


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


# --------------------------------------------------------------------------- #
# target: PR-anchored corpus (x-6ff0)
# --------------------------------------------------------------------------- #

T_NOW = datetime(2026, 7, 18, 12, 0, 0)


def _pr(number, node_id=None, merged="2026-07-10T10:00:00Z", closed=None, repo="o/r", head=None):
    url = f"https://github.com/{repo}/pull/{number}"
    if head is None:
        head = f"feature/{node_id}" if node_id else "hotfix/manual"
    return {"number": number, "headRefName": head, "mergedAt": merged, "closedAt": closed,
            "url": url, "state": "MERGED" if merged else "CLOSED"}


def test_target_partition_and_denominator():
    """AC1-HP: items + unattributed strictly partition the PR list; denominator
    is the PR count, not the ledger row count."""
    nodes = [{"id": "x-a", "reverted": False, "pr_number": 1, "pr_url": "https://github.com/o/r/pull/1"}]
    prs = [_pr(1, "x-a"), _pr(2, None)]  # one attributed, one unattributable
    corpus = fold.build_target_corpus(prs, nodes, [], {}, since_days=28, now=T_NOW)
    cov = corpus["coverage"]
    assert cov["prs_total"] == 2
    assert cov["attributed"] == 1 and cov["unattributed_pr"] == 1
    assert cov["attributed"] + cov["unattributed_pr"] == cov["prs_total"]
    # AC1-EDGE: the unattributable PR is counted with an all-None vector, not dropped
    assert corpus["unattributed"][0]["pr_number"] == 2
    assert corpus["unattributed"][0]["graph_node_id"] is None


def test_target_resolves_via_graph_pr_number_when_branch_absent():
    """Squash-merge drops the branch (headRefName empty): the reverse graph
    pr_number/pr_url lookup recovers the node - branch name alone re-creates the
    52% loss (Domain Pitfall)."""
    nodes = [{"id": "x-sq", "reverted": False, "pr_number": 7, "pr_url": "https://github.com/o/r/pull/7"}]
    prs = [_pr(7, head="")]  # branch gone
    corpus = fold.build_target_corpus(prs, nodes, [], {}, since_days=28, now=T_NOW)
    assert corpus["coverage"]["attributed"] == 1
    assert corpus["items"][0]["graph_node_id"] == "x-sq"


def test_target_outcome_is_reused_classifier():
    """AC2-HP: a fix-node created 5 days post-ship -> bounced, byte-identical to
    what _node_outcome produces (no reimplemented merged/bounced rule)."""
    nodes = [
        {"id": "x-b", "reverted": False, "pr_number": 11, "pr_url": "https://github.com/o/r/pull/11"},
        {"id": "x-fix", "caused_by": "x-b", "created_at": "2026-07-15T10:00:00"},
    ]
    prs = [_pr(11, "x-b", merged="2026-07-11T10:00:00Z")]
    item = fold.build_target_corpus(prs, nodes, [], {}, since_days=28, now=T_NOW)["items"][0]
    assert item["outcome"] == "bounced"
    assert fold.score_target_item(item)["shipped_outcome"] == "degraded"  # attribution unknown -> middle


def test_target_no_w4_outcome_stays_none():
    """AC2-ERR: no causal telemetry graph-wide -> shipped_outcome None; the PR's
    own merged state is never promoted to merged_clean."""
    nodes = [{"id": "x-d", "pr_number": 30, "pr_url": "https://github.com/o/r/pull/30"}]  # no reverted key, no caused_by
    prs = [_pr(30, "x-d")]
    item = fold.build_target_corpus(prs, nodes, [], {}, since_days=28, now=T_NOW)["items"][0]
    assert item["outcome"] is None
    assert fold.score_target_item(item)["shipped_outcome"] is None


def test_target_first_try_green_and_converged_from_structured_signals():
    nodes = [{"id": "x-a", "reverted": False, "pr_number": 1, "pr_url": "https://github.com/o/r/pull/1",
              "sessions": [{"session_id": "sa", "phase": "ship"}]}]
    prs = [_pr(1, "x-a")]
    events = {"sa": [
        {"type": "loop_check", "ts": "2026-07-10T08:00:00Z", "data": {"session_id": "sa", "ci": "SUCCESS", "intent": "promise"}},
        {"type": "termination", "ts": "2026-07-10T09:00:00Z", "data": {"session_id": "sa", "reason": "DonePRGreen"}},
    ]}
    item = fold.build_target_corpus(prs, nodes, [], events, since_days=28, now=T_NOW)["items"][0]
    scores = fold.score_target_item(item)
    assert scores["first_try_green"] == "pass"  # 0 red episodes
    assert scores["converged"] == "pass"        # ship terminal reason
    assert item["signals"]["promises"] == 1 and item["signals"]["loop_fires"] == 1


def test_target_ci_red_episode_fails_first_try_green():
    nodes = [{"id": "x-r", "reverted": False, "pr_number": 5, "pr_url": "https://github.com/o/r/pull/5",
              "sessions": [{"session_id": "sr", "phase": "ship"}]}]
    prs = [_pr(5, "x-r")]
    events = {"sr": [
        {"type": "loop_check", "ts": "2026-07-10T08:00:00Z", "data": {"session_id": "sr", "ci": "FAILURE:unit"}},
        {"type": "loop_check", "ts": "2026-07-10T08:05:00Z", "data": {"session_id": "sr", "ci": "SUCCESS"}},
    ]}
    item = fold.build_target_corpus(prs, nodes, [], events, since_days=28, now=T_NOW)["items"][0]
    assert fold.score_target_item(item)["first_try_green"] == "fail"


def test_target_gc_transcript_no_events_stays_none_no_fabrication():
    """AC2-EDGE: no loop_check joined -> first_try_green None, converged None; no
    process dimension back-filled from the merge; tool-error/permission signals
    absent (not 0)."""
    nodes = [{"id": "x-c", "reverted": False, "caused_by": None, "pr_number": 20,
              "pr_url": "https://github.com/o/r/pull/20"}]
    prs = [_pr(20, "x-c")]
    item = fold.build_target_corpus(prs, nodes, [], {}, since_days=28, now=T_NOW)["items"][0]
    sig = item["signals"]
    assert sig["loop_fires"] is None and sig["ci_reds"] is None and sig["promises"] is None
    assert "tool_errors" not in sig and "permission_denials" not in sig  # absent, never 0
    scores = fold.score_target_item(item)
    assert scores["first_try_green"] is None
    assert scores["converged"] is None  # merged, but no terminal-reason signal -> None
    assert scores["shipped_outcome"] == "pass"  # w4 available (caused_by key) + clean


def test_target_no_pr_class_corrects_survivorship():
    """AC3-HP: a churny build attempt with no PR becomes its own labeled class,
    bucketed by stop-cause; a plan-only thread is excluded."""
    nodes = [{"id": "x-a", "reverted": False, "pr_number": 1, "pr_url": "https://github.com/o/r/pull/1"}]
    prs = [_pr(1, "x-a")]
    rows = [
        {"completed": "2026-07-14T10:00:00", "termination_reason": "NoProgress", "phases_completed": ["do"], "sessions": ["snope"]},
        {"completed": "2026-07-14T11:00:00", "termination_reason": "Aborted", "phases_completed": ["review"], "sessions": ["sabort"]},
        {"completed": "2026-07-14T12:00:00", "termination_reason": "DoneAdvisory", "phases_completed": ["think"], "sessions": ["splan"]},  # plan-only, excluded
    ]
    corpus = fold.build_target_corpus(prs, nodes, rows, {}, since_days=28, now=T_NOW)
    cov = corpus["coverage"]
    assert cov["no_pr_attempts"] == 2
    assert cov["no_pr_stop_cause"] == {"NoProgress": 1, "Aborted": 1}


def test_target_no_pr_excludes_scored_pr_sessions():
    """A ledger row whose session belongs to a scored PR is NOT double-counted as
    a no-PR attempt."""
    nodes = [{"id": "x-a", "reverted": False, "pr_number": 1, "pr_url": "https://github.com/o/r/pull/1",
              "sessions": [{"session_id": "sa", "phase": "ship"}]}]
    prs = [_pr(1, "x-a")]
    rows = [{"completed": "2026-07-14T10:00:00", "termination_reason": "DonePRGreen", "phases_completed": ["ship"], "sessions": ["sa"]}]
    corpus = fold.build_target_corpus(prs, nodes, rows, {}, since_days=28, now=T_NOW)
    assert corpus["coverage"]["no_pr_attempts"] == 0  # sa is the scored PR's own session


def test_target_closed_unmerged_pr_has_no_outcome_label():
    """A closed-but-unmerged PR is a non-delivery: shipped_outcome None (merge
    state is the anchor, and it did not merge), still counted in the denominator."""
    nodes = [{"id": "x-x", "reverted": False, "pr_number": 9, "pr_url": "https://github.com/o/r/pull/9"}]
    prs = [_pr(9, "x-x", merged=None, closed="2026-07-10T10:00:00Z")]
    corpus = fold.build_target_corpus(prs, nodes, [], {}, since_days=28, now=T_NOW)
    item = corpus["items"][0]
    assert item["merged"] is False
    assert item["judgeable"] is False
    assert fold.score_target_item(item)["shipped_outcome"] is None
    assert corpus["coverage"]["attributed"] == 1  # still in the denominator


def test_isolation_violation_detected(tmp_path):
    leaked = "20260704T999999Z-leak"
    real_ledger = tmp_path / "ledger.json"
    real_ledger.write_text('{"entries":[{"session_id":"' + leaked + '","cost_usd":1}]}\n')
    result = isolation.check_isolation({leaked}, {"ledger_json": real_ledger})
    assert result.verdict == "violated"
    assert result.violations[0].session_id == leaked
    # a clean scan (id absent) -> clean
    assert isolation.check_isolation({"not-present"}, {"ledger_json": real_ledger}).verdict == "clean"
