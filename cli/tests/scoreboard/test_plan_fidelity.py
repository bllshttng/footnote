"""AC coverage for `fno scoreboard --plan-fidelity` (x-ed6b3294 / x-68d3).

Grades PLANNING quality by joining a planning thread's plan doc to its
delivery (PR diff + SUMMARY.md). Attributed to the planning session_id.

AC1  a planned row joined to a shipped node emits AC-coverage + scope-drift
     + data-model-surprise, attributed to the planning session_id.
AC2  a delivery touching schema/migration files absent from the ownership map
     scores non-zero data-model-surprise.
AC3  a planned row with no joinable delivery is `unjoined`, never scored 0%.
"""

from __future__ import annotations

from datetime import datetime

from fno.scoreboard.fold import build_plan_fidelity

NOW = datetime(2026, 7, 3, 20, 0, 0)

PLAN_DOC = """
## Acceptance Criteria
#### AC1-HP: ...
#### AC2-ERR: ...

## File Ownership Map
| File | Action | Owner |
|---|---|---|
| `cli/src/fno/scoreboard/fold.py` | modify | /blueprint |
"""


def _fidelity(rows, graph=None, *, plan_doc=PLAN_DOC, summary="", diff=None):
    return build_plan_fidelity(
        rows,
        graph or [],
        since_days=28,
        now=NOW,
        read_plan_doc=lambda p: plan_doc,
        read_summary=lambda row: summary,
        read_diff=lambda pr: diff,
    )


# --- AC1 ---------------------------------------------------------------------
def test_joined_plan_emits_scores_attributed_to_planning_session():
    # Same plan dir, different WORKTREE prefixes: the join is prefix-independent
    # (keys on parent-dir + file), so a plan-thread and a build-thread in
    # separate worktrees still join.
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/wt-a/feat-a/00-INDEX.md",
         "project": "fno", "session_id": "plan-sess", "cost_usd": 2.0},
        {"completed": "2026-07-03T11:00:00", "termination_reason": "DonePRGreen",
         "phases_completed": ["do", "ship"], "plan_path": "/wt-b/feat-a/00-INDEX.md",
         "project": "fno", "graph_node_id": "x-1", "pr_number": 42, "session_id": "build-sess", "cost_usd": 6.0},
    ]
    pf = _fidelity(rows, summary="AC1-HP verified. AC2-ERR verified.",
                   diff=["cli/src/fno/scoreboard/fold.py"])
    joined = [r for r in pf["results"] if r["status"] == "joined"]
    assert len(joined) == 1
    r = joined[0]
    assert r["session_id"] == "plan-sess"  # attributed to the PLANNING session
    assert r["pr_number"] == 42
    assert r["ac_coverage"] == {"verified": 2, "total": 2, "pct": 100}
    assert r["scope_drift"] == {"unplanned": [], "untouched": []}
    assert r["data_model_surprise"] == 0


# --- AC2 ---------------------------------------------------------------------
def test_schema_file_absent_from_map_scores_data_model_surprise():
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/x/plan-a.md",
         "session_id": "plan-sess", "cost_usd": 2.0},
        {"completed": "2026-07-03T11:00:00", "termination_reason": "DonePRGreen",
         "phases_completed": ["do", "ship"], "plan_path": "/x/plan-a.md",
         "graph_node_id": "x-1", "pr_number": 42, "session_id": "build-sess", "cost_usd": 6.0},
    ]
    pf = _fidelity(rows, summary="",
                   diff=["cli/src/fno/scoreboard/fold.py", "db/migrations/0002_add_col.sql"])
    r = [x for x in pf["results"] if x["status"] == "joined"][0]
    assert r["data_model_surprise"] >= 1  # the unplanned .sql migration
    assert "db/migrations/0002_add_col.sql" in r["scope_drift"]["unplanned"]


# --- AC3 ---------------------------------------------------------------------
def test_unjoined_plan_never_scored_zero():
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/x/orphan-plan.md",
         "session_id": "plan-sess", "cost_usd": 2.0},
    ]
    pf = _fidelity(rows)
    assert len(pf["results"]) == 1
    r = pf["results"][0]
    assert r["status"] == "unjoined"
    assert "ac_coverage" not in r  # no fabricated 0%
    assert "data_model_surprise" not in r


def test_coverage_line_reports_joined_pct():
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/x/plan-a.md",
         "session_id": "s1", "cost_usd": 2.0},
        {"completed": "2026-07-03T11:00:00", "termination_reason": "DonePRGreen",
         "phases_completed": ["do", "ship"], "plan_path": "/x/plan-a.md",
         "graph_node_id": "x-1", "pr_number": 42, "session_id": "s2", "cost_usd": 6.0},
        {"completed": "2026-07-03T12:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/x/orphan.md",
         "session_id": "s3", "cost_usd": 1.0},
    ]
    pf = _fidelity(rows, summary="", diff=["cli/src/fno/scoreboard/fold.py"])
    assert pf["coverage"]["planned_rows"] == 2
    assert pf["coverage"]["joined_pct"] == 50


def test_no_data_when_window_empty():
    pf = _fidelity([{"completed": "2020-01-01T00:00:00", "type": "think"}])
    assert pf["state"] == "no_data"


# --- review fixes (gemini PR#317) --------------------------------------------
def test_path_suffix_boundary_no_false_match():
    """`some_other_fold.py` must NOT match owned `.../fold.py` (path-boundary guard)."""
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/x/plan-a.md",
         "session_id": "plan-sess", "cost_usd": 2.0},
        {"completed": "2026-07-03T11:00:00", "termination_reason": "DonePRGreen",
         "phases_completed": ["do", "ship"], "plan_path": "/x/plan-a.md",
         "graph_node_id": "x-1", "pr_number": 42, "session_id": "build-sess", "cost_usd": 6.0},
    ]
    pf = _fidelity(rows, summary="", diff=["cli/src/fno/scoreboard/some_other_fold.py"])
    r = [x for x in pf["results"] if x["status"] == "joined"][0]
    assert "cli/src/fno/scoreboard/some_other_fold.py" in r["scope_drift"]["unplanned"]
    assert "cli/src/fno/scoreboard/fold.py" in r["scope_drift"]["untouched"]


def test_root_level_models_py_is_data_model():
    """A root-level `models.py` (no leading slash) must count as data-model surprise."""
    from fno.scoreboard.fold import _is_data_model_file
    assert _is_data_model_file("models.py")
    assert _is_data_model_file("model.py")
    assert _is_data_model_file("app/models.py")
    assert not _is_data_model_file("cli/src/fno/scoreboard/fold.py")


def test_cross_project_same_basename_does_not_collide():
    """Two `00-INDEX.md` plans in different projects must NOT join to each other."""
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/a/feat/00-INDEX.md",
         "project": "proj-a", "session_id": "plan-a", "cost_usd": 2.0},
        # A shipped row in a DIFFERENT project with the same filename+parent.
        {"completed": "2026-07-03T11:00:00", "termination_reason": "DonePRGreen",
         "phases_completed": ["do", "ship"], "plan_path": "/b/feat/00-INDEX.md",
         "project": "proj-b", "graph_node_id": "x-9", "pr_number": 99, "session_id": "build-b", "cost_usd": 6.0},
    ]
    pf = _fidelity(rows, summary="", diff=["x.py"])
    # The proj-a plan has no delivery IN proj-a -> unjoined, never mis-joined to proj-b.
    plan_a = [r for r in pf["results"] if r["session_id"] == "plan-a"][0]
    assert plan_a["status"] == "unjoined"


def test_read_diff_pins_repo_from_pr_url(monkeypatch):
    """_default_read_diff must pass --repo derived from the delivery row's pr_url."""
    import subprocess as _sp
    from fno.scoreboard import fold

    captured = {}

    class _Done:
        returncode = 0
        stdout = "a.py\nb.py\n"

    def _fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _Done()

    monkeypatch.setattr(_sp, "run", _fake_run)
    files = fold._default_read_diff(
        {"pr_number": 42, "pr_url": "https://github.com/acme/widgets/pull/42"}
    )
    assert files == ["a.py", "b.py"]
    assert "--repo" in captured["cmd"]
    assert "acme/widgets" in captured["cmd"]


def test_read_diff_no_pr_number_returns_none():
    from fno.scoreboard import fold
    assert fold._default_read_diff({"pr_number": None}) is None
