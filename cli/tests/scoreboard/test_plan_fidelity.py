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
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/x/plan-a.md",
         "session_id": "plan-sess", "cost_usd": 2.0},
        {"completed": "2026-07-03T11:00:00", "termination_reason": "DonePRGreen",
         "phases_completed": ["do", "ship"], "plan_path": "/other/plan-a.md",
         "graph_node_id": "x-1", "pr_number": 42, "session_id": "build-sess", "cost_usd": 6.0},
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
