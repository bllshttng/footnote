"""Event-processing core coverage (idempotency, tolerant read, no-op, local-maxima)."""
from __future__ import annotations

from fno.skill_diff import engine


def _rc(run_id, skill_id="fno:blueprint", top="structural_validity", fails=2):
    return {
        "type": "skill_eval_run_complete",
        "data": {"run_id": run_id, "skill_id": skill_id,
                 "failure_ranking": [{"dimension": top, "fail_count": fails}]},
    }


def _finding(run_id, verdict="fail", dim="structural_validity", tool_fault=False, skill_id="fno:blueprint"):
    d = {"run_id": run_id, "skill_id": skill_id, "dimension": dim, "verdict": verdict}
    if tool_fault:
        d["tool_fault"] = True
    return {"type": "skill_eval_finding", "data": d}


def test_read_events_tolerant_skips_corrupt(tmp_path):  # AC3-ERR
    p = tmp_path / "events.jsonl"
    p.write_text('{"type":"a"}\nNOT JSON\n\n{"type":"b"}\n')
    out = engine.read_events_tolerant(p)
    assert [e["type"] for e in out] == ["a", "b"]


def test_read_events_missing_file_is_empty(tmp_path):
    assert engine.read_events_tolerant(tmp_path / "nope.jsonl") == []


def test_unprocessed_and_idempotency():  # AC8-FR
    evs = [_rc("r1"), _rc("r2")]
    assert engine.unprocessed_runs(evs, "fno:blueprint") == ["r1", "r2"]
    evs.append({"type": "skill_diff_proposed", "data": {"run_id": "r1", "skill_id": "fno:blueprint"}})
    assert engine.unprocessed_runs(evs, "fno:blueprint") == ["r2"]
    evs.append({"type": "skill_diff_noop", "data": {"run_id": "r2", "skill_id": "fno:blueprint"}})
    assert engine.unprocessed_runs(evs, "fno:blueprint") == []


def test_unprocessed_scopes_by_skill():
    evs = [_rc("r1", skill_id="fno:blueprint"), _rc("r2", skill_id="fno:review")]
    assert engine.unprocessed_runs(evs, "fno:blueprint") == ["r1"]
    assert engine.unprocessed_runs(evs, "fno:review") == ["r2"]


def test_findings_exclude_tool_fault():  # tool_fault rule
    evs = [_finding("r1"), _finding("r1", tool_fault=True), _finding("r1", verdict="pass")]
    got = engine.findings_for_run(evs, "r1")
    assert len(got) == 2 and all(f.get("tool_fault") is not True for f in got)


def test_has_actionable_findings():  # AC6-EDGE
    assert engine.has_actionable_findings([_finding("r1", verdict="fail")], "r1")
    assert engine.has_actionable_findings([_finding("r1", verdict="degraded")], "r1")
    assert not engine.has_actionable_findings([_finding("r1", verdict="pass")], "r1")
    assert not engine.has_actionable_findings([], "r1")  # zero findings


def test_failure_ranking_prefers_run_complete():
    evs = [_rc("r1", top="collision_free", fails=5)]
    assert engine.top_dimension(evs, "r1") == "collision_free"


def test_failure_ranking_falls_back_to_counting():
    evs = [
        _finding("r1", verdict="fail", dim="structural_validity"),
        _finding("r1", verdict="fail", dim="structural_validity"),
        _finding("r1", verdict="fail", dim="collision_free"),
    ]
    # run_complete without a ranking -> recount, excluding tool faults
    evs.insert(0, {"type": "skill_eval_run_complete", "data": {"run_id": "r1", "skill_id": "fno:blueprint"}})
    assert engine.top_dimension(evs, "r1") == "structural_validity"


def test_local_maxima_trips_only_with_proposal_in_span():  # AC7-EDGE
    # Three consecutive runs, same top dimension, one proposal in between -> trip.
    evs = [_rc("r1"), _rc("r2"), _rc("r3"),
           {"type": "skill_diff_proposed", "data": {"run_id": "r2", "skill_id": "fno:blueprint"}}]
    assert engine.local_maxima_tripped(evs, "fno:blueprint", "r3")


def test_local_maxima_does_not_trip_without_proposal():
    evs = [_rc("r1"), _rc("r2"), _rc("r3")]
    assert not engine.local_maxima_tripped(evs, "fno:blueprint", "r3")


def test_local_maxima_does_not_trip_when_dimension_changed():
    evs = [_rc("r1", top="collision_free"), _rc("r2"), _rc("r3"),
           {"type": "skill_diff_proposed", "data": {"run_id": "r2", "skill_id": "fno:blueprint"}}]
    assert not engine.local_maxima_tripped(evs, "fno:blueprint", "r3")


def test_local_maxima_needs_full_window():
    evs = [_rc("r1"), _rc("r2"),
           {"type": "skill_diff_proposed", "data": {"run_id": "r1", "skill_id": "fno:blueprint"}}]
    assert not engine.local_maxima_tripped(evs, "fno:blueprint", "r2")  # only 2 < window 3


def test_build_pr_body_records_both_hashes_when_drifted():  # AC9-FR
    body = engine.build_pr_body(
        run_id="r1", skill_id="fno:blueprint",
        hunks=[{"file": "skills/blueprint/SKILL.md", "cited_finding_ids": ["s1"], "rationale": "why"}],
        justification=None, bloat=None,
        version_observed="aaa", version_against="bbb", is_review_skill=False,
    )
    assert "`aaa`" in body and "`bbb`" in body
    assert "moved between eval and proposal" in body
    assert "s1" in body


def test_build_pr_body_renders_justification_and_bloat():
    body = engine.build_pr_body(
        run_id="r1", skill_id="fno:blueprint",
        hunks=[{"file": "f", "cited_finding_ids": ["s1"]}],
        justification="needed because X", bloat={"net_growth": 200, "window": 5, "threshold": 120},
        version_observed="a", version_against="a", is_review_skill=False,
    )
    assert "Justification" in body and "needed because X" in body
    assert "bloat_review_needed" in body and "+200" in body


def test_build_pr_body_flags_review_skill_replay_gap():
    body = engine.build_pr_body(
        run_id="r1", skill_id="fno:review",
        hunks=[{"file": "f", "cited_finding_ids": ["s1"]}],
        justification=None, bloat=None,
        version_observed="a", version_against="a", is_review_skill=True,
    )
    assert "Replay v1 covers /blueprint only" in body
