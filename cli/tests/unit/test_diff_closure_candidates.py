"""Diff-derived closure candidates: the merge-diff edge defect A names.

The only pr-to-node edge before this was the branch name
(_branch_matches_node). Most open p1 nodes carry no plan_path, so a second,
weaker edge - file overlap between a merge's changed files and what a node
actually records (a plan's Files to Modify table, or path-shaped tokens in
details) - surfaces a candidate a human confirms. Never an auto-close.

Filter: ``fno doctor test cli/tests/unit/test_diff_closure_candidates.py``
"""
from __future__ import annotations

import subprocess

from fno.graph._reconcile import (
    DiffCandidate,
    NodeProbeVerdict,
    PrMergeState,
    ReconcileError,
    confirm_diff_candidate,
    diff_closure_candidates,
    evaluate_node_closure_probe,
    node_declared_surfaces,
    reconcile_diff_candidates,
)


def _node(node_id: str, **over) -> dict:
    base = {"id": node_id, "title": f"node {node_id}", "completed_at": None}
    base.update(over)
    return base


# -- node_declared_surfaces --


def test_details_yields_backtick_fenced_paths_only():
    node = _node(
        "x-1",
        details="Fix in `cli/src/fno/plan/fidelity.py`. Not a path: fidelity.py alone, "
        "or `just words`. Also see `docs/architecture/thing.md`.",
    )
    assert node_declared_surfaces(node) == {
        "cli/src/fno/plan/fidelity.py",
        "docs/architecture/thing.md",
    }


def test_a_plan_path_that_cannot_be_read_falls_through_to_details(tmp_path):
    node = _node(
        "x-1",
        plan_path=str(tmp_path / "does-not-exist.md"),
        details="Touches `cli/src/fno/plan/fidelity.py`.",
    )
    assert node_declared_surfaces(node) == {"cli/src/fno/plan/fidelity.py"}


def test_a_readable_plan_wins_over_details(tmp_path, monkeypatch):
    import fno.graph.collision as collision

    plan = tmp_path / "plan.md"
    plan.write_text(
        "## Files to Modify\n\n| File | Action |\n|---|---|\n"
        "| `cli/src/fno/plan/from_table.py` | Modify |\n"
    )
    monkeypatch.setattr(collision, "_find_repo_root", lambda: tmp_path)
    node = _node(
        "x-1", plan_path=str(plan), details="Also mentions `cli/src/fno/other.py`."
    )
    assert node_declared_surfaces(node) == {"cli/src/fno/plan/from_table.py"}


# -- diff_closure_candidates --


def test_an_overlapping_open_node_is_a_candidate():
    entries = [_node("x-1", details="Fix in `cli/src/fno/plan/fidelity.py`.")]
    candidates = diff_closure_candidates(
        entries, pr_number=42, pr_url="https://github.com/o/r/pull/42",
        changed_files=["cli/src/fno/plan/fidelity.py", "docs/x.md"],
    )
    assert candidates == [
        DiffCandidate(
            node_id="x-1", pr_number=42, pr_url="https://github.com/o/r/pull/42",
            overlapping_paths=["cli/src/fno/plan/fidelity.py"],
        )
    ]


def test_a_node_already_matched_by_branch_is_excluded():
    entries = [_node("x-1", details="Fix in `cli/src/fno/plan/fidelity.py`.")]
    candidates = diff_closure_candidates(
        entries, pr_number=42, pr_url=None,
        changed_files=["cli/src/fno/plan/fidelity.py"],
        matched_node_ids={"x-1"},
    )
    assert candidates == []


def test_no_overlap_reports_nothing():
    entries = [_node("x-1", details="Fix in `cli/src/fno/other.py`.")]
    candidates = diff_closure_candidates(
        entries, pr_number=42, pr_url=None, changed_files=["docs/unrelated.md"],
    )
    assert candidates == []


def test_a_closed_node_is_never_a_candidate():
    entries = [
        _node(
            "x-1", completed_at="2026-08-01T00:00:00+00:00",
            details="Fix in `cli/src/fno/plan/fidelity.py`.",
        )
    ]
    candidates = diff_closure_candidates(
        entries, pr_number=42, pr_url=None,
        changed_files=["cli/src/fno/plan/fidelity.py"],
    )
    assert candidates == []


# -- reconcile_diff_candidates --


def test_a_branch_matched_merge_produces_no_candidate_and_skips_the_file_fetch():
    entries = [_node("x-5b66", details="Fix in `cli/src/fno/other.py`.")]
    fetch_calls: list[int] = []

    def files_reader(number, *, cwd, include_files):
        fetch_calls.append(number)
        return PrMergeState(number=number, state="MERGED", url=None, merged_at=None)

    def list_merged(*, cwd):
        return [{"number": 7, "url": "u", "headRefName": "feature/x-5b66", "mergedAt": "t"}]

    out = reconcile_diff_candidates(
        entries, cwd="/repo", list_merged=list_merged, merge_state_reader=files_reader
    )
    assert out == []
    assert fetch_calls == [], "a branch-matched merge must never pay a file fetch"


def test_an_unmatched_merge_touching_a_surface_produces_a_candidate():
    entries = [_node("x-1", details="Fix in `cli/src/fno/plan/fidelity.py`.")]

    def merge_state_reader(number, *, cwd, include_files):
        return PrMergeState(
            number=number, state="MERGED", url="https://github.com/o/r/pull/9",
            merged_at="t", changed_files=["cli/src/fno/plan/fidelity.py"],
        )

    def list_merged(*, cwd):
        return [{"number": 9, "url": "u", "headRefName": "feature/unrelated-branch", "mergedAt": "t"}]

    out = reconcile_diff_candidates(
        entries, cwd="/repo", list_merged=list_merged, merge_state_reader=merge_state_reader
    )
    assert [c.node_id for c in out] == ["x-1"]
    assert out[0].pr_number == 9


def test_a_merge_touching_nothing_reports_an_empty_list_not_an_exception():
    entries = [_node("x-1", details="Fix in `cli/src/fno/other.py`.")]

    def merge_state_reader(number, *, cwd, include_files):
        return PrMergeState(number=number, state="MERGED", url=None, merged_at="t", changed_files=["docs/unrelated.md"])

    def list_merged(*, cwd):
        return [{"number": 3, "url": None, "headRefName": "feature/nope", "mergedAt": "t"}]

    out = reconcile_diff_candidates(
        entries, cwd="/repo", list_merged=list_merged, merge_state_reader=merge_state_reader
    )
    assert out == []


def test_a_gh_outage_on_the_list_call_degrades_to_empty():
    def list_merged(*, cwd):
        raise ReconcileError("gh down")

    out = reconcile_diff_candidates([_node("x-1")], cwd="/repo", list_merged=list_merged)
    assert out == []


# -- confirm_diff_candidate --


def test_confirming_a_candidate_binds_the_pr_and_stamps_the_derivation():
    entries = [_node("x-1abc0000", details="Fix in `cli/src/fno/plan/fidelity.py`.")]
    result = confirm_diff_candidate(
        entries, "x-1abc0000", pr_number=9, pr_url="https://github.com/o/r/pull/9",
        confirmed_by="a-human",
    )
    assert result.outcome == "bound"
    node = entries[0]
    assert node["pr_number"] == 9
    assert node["pr_edge_derivation"] == {
        "via": "diff-candidate",
        "pr_number": 9,
        "confirmed_by": "a-human",
        "confirmed_at": node["pr_edge_derivation"]["confirmed_at"],
    }


def test_confirming_an_unknown_node_refuses_and_stamps_nothing():
    entries = [_node("x-1abc0000")]
    result = confirm_diff_candidate(entries, "x-99990000", pr_number=9, pr_url=None)
    assert result.outcome == "refused"
    assert "pr_edge_derivation" not in entries[0]


# -- evaluate_node_closure_probe (change 5, opt-in) --


def test_no_probe_declared_returns_none():
    assert evaluate_node_closure_probe(_node("x-1")) is None


def test_a_passing_probe_reports_passed_true():
    verdict = evaluate_node_closure_probe(_node("x-1", closure_probe="true"))
    assert verdict == NodeProbeVerdict(True, "")


def test_a_failing_probe_fails_closed_and_names_the_exit():
    verdict = evaluate_node_closure_probe(_node("x-1", closure_probe="exit 3"))
    assert verdict is not None
    assert verdict.passed is False
    assert "exited 3" in verdict.detail


def test_a_probe_that_times_out_fails_closed():
    def timing_out_runner(*a, **kw):
        raise subprocess.TimeoutExpired(cmd="sleep", timeout=30)

    verdict = evaluate_node_closure_probe(
        _node("x-1", closure_probe="sleep 999"), runner=timing_out_runner
    )
    assert verdict is not None
    assert verdict.passed is False
    assert "timed out" in verdict.detail


def test_a_probe_that_cannot_launch_fails_closed():
    def unlaunchable_runner(*a, **kw):
        raise OSError("no such binary")

    verdict = evaluate_node_closure_probe(
        _node("x-1", closure_probe="does-not-exist"), runner=unlaunchable_runner
    )
    assert verdict is not None
    assert verdict.passed is False
    assert "failed to launch" in verdict.detail


# -- probe wiring in diff_closure_candidates / reconcile_diff_candidates --


def test_a_candidate_with_no_probe_carries_none():
    entries = [_node("x-1", details="Fix in `cli/src/fno/plan/fidelity.py`.")]
    out = diff_closure_candidates(
        entries, pr_number=1, pr_url=None, changed_files=["cli/src/fno/plan/fidelity.py"],
    )
    assert out[0].probe is None


def test_a_candidate_with_a_declared_probe_carries_its_verdict():
    entries = [
        _node(
            "x-1", details="Fix in `cli/src/fno/plan/fidelity.py`.",
            closure_probe="true",
        )
    ]
    out = diff_closure_candidates(
        entries, pr_number=1, pr_url=None, changed_files=["cli/src/fno/plan/fidelity.py"],
    )
    assert out[0].probe == NodeProbeVerdict(True, "")


def test_reconcile_diff_candidates_threads_the_probe_runner_through():
    entries = [
        _node(
            "x-1", details="Fix in `cli/src/fno/plan/fidelity.py`.",
            closure_probe="anything",
        )
    ]

    def fake_probe_runner(*a, **kw):
        raise OSError("stubbed: never actually shells out in this test")

    def merge_state_reader(number, *, cwd, include_files):
        return PrMergeState(
            number=number, state="MERGED", url="u", merged_at="t",
            changed_files=["cli/src/fno/plan/fidelity.py"],
        )

    def list_merged(*, cwd):
        return [{"number": 1, "url": "u", "headRefName": "feature/unrelated", "mergedAt": "t"}]

    out = reconcile_diff_candidates(
        entries, cwd="/repo", list_merged=list_merged,
        merge_state_reader=merge_state_reader, probe_runner=fake_probe_runner,
    )
    assert out[0].probe.passed is False
    assert "failed to launch" in out[0].probe.detail
