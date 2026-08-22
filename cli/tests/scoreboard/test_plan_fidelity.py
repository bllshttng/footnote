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

import json
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


# --- AC3-HP: done_probes join declaration to evidence (x-e54c) ----------------
_PROBE_PLAN = (
    "---\ntitle: p\ndone_probes:\n"
    '  - "fno agents mail list --since 24h | grep -q groom"\n'
    '  - "test -n \\"$(fno backlog groom --status)\\""\n'
    "---\n\n# plan\n\n## Acceptance Criteria\n"
)

_PROBE_ROWS = [
    {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
     "phases_completed": ["think", "plan"], "plan_path": "/wt-a/feat/00-INDEX.md",
     "project": "fno", "session_id": "plan-sess", "cost_usd": 2.0},
    {"completed": "2026-07-03T12:00:00", "termination_reason": "DonePRGreen",
     "phases_completed": ["do", "ship"], "plan_path": "/wt-b/feat/00-INDEX.md",
     "project": "fno", "graph_node_id": "x-1", "pr_number": 7,
     "session_id": "build-sess", "cost_usd": 6.0},
]


def _probe_event(session_id, probes):
    return {"type": "loop_check", "data": {"session_id": session_id, "done_probes": probes}}


def test_probes_join_declaration_to_delivery_evidence():
    events = [_probe_event("build-sess", {
        "fno agents mail list --since 24h | grep -q groom": "pass",
        'test -n "$(fno backlog groom --status)"': "pass",
    })]
    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=events,
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 2}


def test_probes_count_only_passes_not_declarations():
    events = [_probe_event("build-sess", {
        "fno agents mail list --since 24h | grep -q groom": "fail:1",
        'test -n "$(fno backlog groom --status)"': "pass",
    })]
    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=events,
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 1}


def test_probes_declared_but_no_evidence_reports_zero_passed():
    """A declared probe with no recorded fire is 0 passed, not null: the
    declaration is real, the evidence is simply missing."""
    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=[],
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 0}


def test_probes_null_when_plan_declares_none():
    """Coverage honesty: no declaration is unmeasurable, never 0/0 or {}."""
    pf = _fidelity(_PROBE_ROWS, summary="", diff=[])
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] is None
    assert "probes" in joined, "the key must be present, never omitted"


def test_probe_evidence_takes_the_last_fire():
    """A session that blocked on a failing probe then passed on a later fire is
    graded on the fire that granted done."""
    events = [
        _probe_event("build-sess", {"fno agents mail list --since 24h | grep -q groom": "fail:1",
                                    'test -n "$(fno backlog groom --status)"': "fail:1"}),
        _probe_event("build-sess", {"fno agents mail list --since 24h | grep -q groom": "pass",
                                    'test -n "$(fno backlog groom --status)"': "pass"}),
    ]
    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=events,
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 2}


def test_probe_evidence_ignores_another_sessions_events():
    events = [_probe_event("some-other-sess", {
        "fno agents mail list --since 24h | grep -q groom": "pass",
        'test -n "$(fno backlog groom --status)"': "pass",
    })]
    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=events,
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 0}


def test_probe_evidence_reads_the_real_events_file_shape(tmp_path):
    """The production wiring: build_plan_fidelity is fed by read_jsonl_events.

    Every other probe test injects events directly, so a shape mismatch here
    (unwrapped `data`, wrong kind key) would leave them all green while the
    scoreboard silently reported 0 passed for every plan - indistinguishable
    from the real signal this feature exists to produce.
    """
    from fno.scoreboard.fold import read_jsonl_events

    events_file = tmp_path / "events.jsonl"
    events_file.write_text(
        json.dumps({
            "type": "loop_check",
            "data": {
                "session_id": "build-sess",
                "done_probes": {
                    "fno agents mail list --since 24h | grep -q groom": "pass",
                    'test -n "$(fno backlog groom --status)"': "pass",
                },
            },
        })
        + "\n",
        encoding="utf-8",
    )

    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=read_jsonl_events([events_file], {"loop_check"}),
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 2}


def test_undeterminable_marker_counts_as_zero_passed_not_null():
    """A refusal where nothing ran records a marker, not command results. The
    plan still declared probes, so the grader must report 0 passed."""
    events = [_probe_event("build-sess", {"_undeterminable": "over-cap"})]
    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=events,
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 0}


def test_probe_evidence_skips_a_corrupt_event_without_crashing():
    """A malformed `data` (string/list/None) must skip, not raise - one bad
    line in events.jsonl would otherwise take down the whole scoreboard verb."""
    events = [
        {"type": "loop_check", "data": "not-a-dict"},
        {"type": "loop_check", "data": None},
        {"type": "loop_check", "data": ["also", "not", "a", "dict"]},
        _probe_event("build-sess", {
            "fno agents mail list --since 24h | grep -q groom": "pass",
            'test -n "$(fno backlog groom --status)"': "pass",
        }),
    ]
    pf = build_plan_fidelity(
        _PROBE_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: _PROBE_PLAN,
        read_summary=lambda row: "",
        read_diff=lambda pr: [],
        loop_check_events=events,
    )
    joined = [r for r in pf["results"] if r["status"] == "joined"][0]
    assert joined["probes"] == {"declared": 2, "passed": 2}


# --- AC4-CORE: one join, two dispositions (x-cbab) ---------------------------
#
# The unmeasurable knob is the inversion, never a second join. 'unjoined' (the
# default) is byte-identical to today's telemetry; 'refuse' adds a gate key that
# marks unjoined planned rows as a would-be refusal. The carveout waiver that can
# overturn would_refuse is tested via fno.plan.fidelity.fidelity_refusal.

_ORPHAN_ROWS = [
    {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
     "phases_completed": ["think", "plan"], "plan_path": "/x/orphan.md",
     "session_id": "plan-sess", "cost_usd": 2.0},
]


def test_unmeasurable_unjoined_is_byte_identical_to_the_default():
    """The telemetry disposition never sees a gate key, so the scoreboard output
    is unchanged. Default == explicit 'unjoined'."""
    default = _fidelity(_ORPHAN_ROWS)
    explicit = build_plan_fidelity(
        _ORPHAN_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: PLAN_DOC, read_summary=lambda r: "",
        read_diff=lambda pr: None, unmeasurable="unjoined",
    )
    assert default == explicit
    assert "gate" not in default, "telemetry must not carry the gate key"


def test_refuse_disposition_marks_unjoined_as_a_would_be_refusal():
    """The same join, but an unjoined planned row is a refusal rather than n/a."""
    pf = build_plan_fidelity(
        _ORPHAN_ROWS, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: PLAN_DOC, read_summary=lambda r: "",
        read_diff=lambda pr: None, unmeasurable="refuse",
    )
    gate = pf["gate"]
    assert gate["would_refuse"] is True
    assert gate["unjoined_count"] == 1
    assert gate["planned"] == 1 and gate["delivered"] == 0
    # the join itself is unchanged: the unjoined row is still reported.
    assert pf["results"][0]["status"] == "unjoined"


def test_refuse_disposition_passes_when_every_planned_row_joined():
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoWork",
         "phases_completed": ["think", "plan"], "plan_path": "/x/plan-a.md",
         "session_id": "s1", "cost_usd": 2.0},
        {"completed": "2026-07-03T11:00:00", "termination_reason": "DonePRGreen",
         "phases_completed": ["do", "ship"], "plan_path": "/x/plan-a.md",
         "graph_node_id": "x-1", "pr_number": 42, "session_id": "s2", "cost_usd": 6.0},
    ]
    pf = build_plan_fidelity(
        rows, [], since_days=28, now=NOW,
        read_plan_doc=lambda p: PLAN_DOC, read_summary=lambda r: "",
        read_diff=lambda pr: [], unmeasurable="refuse",
    )
    assert pf["gate"]["would_refuse"] is False
    assert pf["gate"]["unjoined_count"] == 0


def test_unknown_unmeasurable_raises_rather_than_silently_defaulting():
    import pytest

    with pytest.raises(ValueError, match="unmeasurable"):
        build_plan_fidelity(
            _ORPHAN_ROWS, [], since_days=28, now=NOW,
            read_plan_doc=lambda p: PLAN_DOC, read_summary=lambda r: "",
            read_diff=lambda pr: None, unmeasurable="bogus",
        )


def test_fidelity_refusal_refuses_an_uncovered_shortfall():
    """AC5 heart: a planned-minus-delivered row with no covering carveout refuses."""
    from fno.plan.fidelity import fidelity_refusal

    unjoined = [{"session_id": "s1", "plan_path": "/x/p.md"}]
    decision = fidelity_refusal(unjoined_rows=unjoined, carveouts=[])
    assert decision["refused"] is True
    assert decision["shortfall"] == 1
    assert decision["covered"] is False
    assert "not a carveout" in decision["reason"]


def test_fidelity_refusal_passes_when_carveouts_cover_the_shortfall():
    from fno.plan.fidelity import fidelity_refusal

    unjoined = [{"session_id": "s1", "plan_path": "/x/p.md"}]
    decision = fidelity_refusal(unjoined_rows=unjoined, carveouts=[{"id": "c1"}])
    assert decision["refused"] is False
    assert decision["covered"] is True


def test_fidelity_refusal_passes_when_there_is_no_shortfall():
    from fno.plan.fidelity import fidelity_refusal

    decision = fidelity_refusal(unjoined_rows=[], carveouts=[])
    assert decision["refused"] is False


def test_compute_plan_fidelity_scopes_selection_to_this_repo(monkeypatch, tmp_path):
    """The ledger is global and two projects can share a plan-path tail.
    compute_plan_fidelity selects only this repo's rows via the same remote-slug
    derivation the ledger stamps, so a foreign project's same-tail plan cannot
    cover or pollute the gate decision. The scoping happens on the INPUT rows,
    before the fold ever runs (x-8ad8) - the fake fold below mirrors the real
    one's output shape (plan_path/session_id, never project - fold.py's
    results.append never copies that field through) so this test actually
    exercises the pre-filter instead of a re-check that real output can't
    satisfy."""
    import fno.paths as paths
    import fno.plan.fidelity as fid
    import fno.scoreboard.fold as fold

    plan = tmp_path / "feat" / "00-INDEX.md"
    plan.parent.mkdir(parents=True)
    plan.write_text("# plan\n")

    def fake_build_plan_fidelity(rows, graph_nodes, **kwargs):
        # Mirrors the real fold: one unjoined result per row it was actually
        # given, output rows carry no "project" key.
        return {
            "state": "ok",
            "results": [
                {"plan_path": r["plan_path"], "status": "unjoined", "session_id": r["session_id"]}
                for r in rows
            ],
        }

    ledger_rows = [
        {"plan_path": "/a/feat/00-INDEX.md", "project": "footnote", "session_id": "s-local"},
        {"plan_path": "/b/feat/00-INDEX.md", "project": "abilities", "session_id": "s-foreign"},
    ]
    # build_plan_fidelity is imported lazily inside compute_plan_fidelity, so
    # patch it at its source module rather than on fid.
    monkeypatch.setattr(fold, "build_plan_fidelity", fake_build_plan_fidelity)
    monkeypatch.setattr(fold, "load_ledger_rows", lambda *a, **k: ledger_rows)
    monkeypatch.setattr(fid, "_load_graph_nodes", lambda: [])
    monkeypatch.setattr(fid, "_read_covering_carveouts", lambda *a, **k: [])
    monkeypatch.setattr(paths, "_slug_from_git_remote", lambda root=None: "footnote")

    decision = fid.compute_plan_fidelity(plan_path=str(plan))

    # Only the footnote row is selected; the abilities row does not count.
    assert decision["planned"] == 1
    assert decision["refused"] is True
    assert decision["unjoined"][0]["session_id"] == "s-local"


def test_compute_plan_fidelity_scopes_ledger_rows_before_the_fold(monkeypatch, tmp_path):
    """x-8ad8: the fold shells a real `gh pr diff` per joined delivery row it is
    given, so folding the WHOLE global ledger for a single-plan query pays for a
    live network call per joined row across every plan ever shipped - the
    reproduced hang (exit 124, zero bytes, no matter the timeout). The fix scopes
    `rows` to this plan BEFORE calling build_plan_fidelity, not just its output
    after; assert the fold never sees an unrelated plan's row at all."""
    import fno.paths as paths
    import fno.plan.fidelity as fid
    import fno.scoreboard.fold as fold

    plan = tmp_path / "feat" / "00-INDEX.md"
    plan.parent.mkdir(parents=True)
    plan.write_text("# plan\n")

    seen_rows = []

    def spy_build_plan_fidelity(rows, graph_nodes, **kwargs):
        seen_rows.extend(rows)
        return {"state": "ok", "results": []}

    ledger_rows = [
        {"plan_path": str(plan), "project": "footnote", "session_id": "s-mine",
         "completed": "2026-08-01T00:00:00", "termination_reason": "none"},
    ] + [
        {"plan_path": f"/other/plan-{i}.md", "project": "footnote",
         "session_id": f"s-other-{i}", "completed": "2026-08-01T00:00:00",
         "termination_reason": "none"}
        for i in range(50)
    ]
    monkeypatch.setattr(fold, "build_plan_fidelity", spy_build_plan_fidelity)
    monkeypatch.setattr(fid, "_load_graph_nodes", lambda: [])
    monkeypatch.setattr(fid, "_read_covering_carveouts", lambda *a, **k: [])
    monkeypatch.setattr(paths, "_slug_from_git_remote", lambda root=None: "footnote")
    monkeypatch.setattr(fold, "load_ledger_rows", lambda *a, **k: ledger_rows)

    fid.compute_plan_fidelity(plan_path=str(plan))

    # Only this plan's own row reached the fold; the 50 unrelated rows never did.
    assert len(seen_rows) == 1
    assert seen_rows[0]["session_id"] == "s-mine"
