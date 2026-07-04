"""tick + reconcile wiring: state-word contract, level gating, idempotency.

The two non-deterministic seams (synthesis agent, gh/git PR open) and the
node-filing subprocess are injected, so no test spawns an agent or touches the
network. Coverage targets the deterministic decision surface: paused, no-work,
no-op, report-level dry-run, local-maxima, and the idempotency invariant.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.skill_diff import cli

runner = CliRunner()


def _wire(monkeypatch, tmp_path, events, *, paused=False, level="report"):
    p = tmp_path / "events.jsonl"
    p.write_text("".join(json.dumps(e) + "\n" for e in events))
    monkeypatch.setattr(cli, "_events_paths", lambda: [p])
    monkeypatch.setattr(cli, "loops_paused", lambda: paused)
    monkeypatch.setattr(cli, "loop_level", lambda name: level)
    return p


def _events(path):
    return [json.loads(ln) for ln in path.read_text().splitlines() if ln.strip()]


def _rc(run_id, top="structural_validity"):
    return {"type": "skill_eval_run_complete",
            "data": {"run_id": run_id, "skill_id": "fno:blueprint",
                     "skill_version": "abc", "failure_ranking": [{"dimension": top, "fail_count": 2}]}}


def _finding(run_id, verdict="fail"):
    return {"type": "skill_eval_finding",
            "data": {"run_id": run_id, "skill_id": "fno:blueprint",
                     "dimension": "structural_validity", "verdict": verdict}}


def test_paused_exits_zero_with_word(monkeypatch, tmp_path):
    p = _wire(monkeypatch, tmp_path, [_rc("r1"), _finding("r1")], paused=True)
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert r.exit_code == 0 and "paused" in r.output
    types = [e["type"] for e in _events(p)]
    assert types.count("loop_tick") == 1
    assert "skill_diff_proposed" not in types


def test_no_work_when_no_runs(monkeypatch, tmp_path):
    _wire(monkeypatch, tmp_path, [])
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert r.exit_code == 0 and "no-work" in r.output


def test_noop_on_all_pass_run(monkeypatch, tmp_path):  # AC6-EDGE
    p = _wire(monkeypatch, tmp_path, [_rc("r1"), _finding("r1", verdict="pass")])
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert r.exit_code == 0 and "noop" in r.output
    noops = [e for e in _events(p) if e["type"] == "skill_diff_noop"]
    assert noops and noops[0]["data"]["run_id"] == "r1"


def test_report_level_is_dry_run(monkeypatch, tmp_path):  # report is the default level
    # synthesize must NOT be called at report level.
    called = {"n": 0}
    monkeypatch.setattr(cli.synthesize, "synthesize", lambda *a, **k: called.__setitem__("n", 1))
    p = _wire(monkeypatch, tmp_path, [_rc("r1"), _finding("r1")], level="report")
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert r.exit_code == 0 and "report" in r.output
    assert called["n"] == 0
    assert "skill_diff_proposed" not in [e["type"] for e in _events(p)]


def test_idempotent_after_proposed(monkeypatch, tmp_path):  # AC8-FR
    events = [_rc("r1"), _finding("r1"),
              {"type": "skill_diff_proposed", "data": {"run_id": "r1", "skill_id": "fno:blueprint"}}]
    _wire(monkeypatch, tmp_path, events)
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert "no-work" in r.output  # r1 already handled


def test_local_maxima_files_node(monkeypatch, tmp_path):  # AC7-EDGE
    monkeypatch.setattr(cli, "_file_no_diff_node", lambda *a, **k: "fno-dead")
    # r1/r2 already handled (proposed) so r3 is the oldest unprocessed run; the
    # window r1..r3 shares one top dimension with proposals in the span -> trip.
    events = [_rc("r1"), _rc("r2"), _rc("r3"), _finding("r3"),
              {"type": "skill_diff_proposed", "data": {"run_id": "r1", "skill_id": "fno:blueprint"}},
              {"type": "skill_diff_proposed", "data": {"run_id": "r2", "skill_id": "fno:blueprint"}}]
    p = _wire(monkeypatch, tmp_path, events, level="assisted")
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert "no-diff-helps" in r.output and "local_maxima" in r.output
    ndh = [e for e in _events(p) if e["type"] == "skill_diff_no_diff_helps"]
    assert ndh and ndh[0]["data"]["filed_node_id"] == "fno-dead"


def test_assisted_no_diff_helps_when_synth_declines(monkeypatch, tmp_path):
    from fno.skill_diff import synthesize as s
    monkeypatch.setattr(cli, "_file_no_diff_node", lambda *a, **k: "fno-x")
    monkeypatch.setattr(cli.synthesize, "synthesize",
                        lambda *a, **k: s.Proposal(verdict="no_diff_helps", no_diff_reason="arch"))
    p = _wire(monkeypatch, tmp_path, [_rc("r1"), _finding("r1")], level="assisted")
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert "no-diff-helps" in r.output
    ndh = [e for e in _events(p) if e["type"] == "skill_diff_no_diff_helps"]
    assert ndh[0]["data"]["reason"] == "synth_no_diff"


def test_assisted_uncited_hunks_take_no_diff_path(monkeypatch, tmp_path):  # AC2-ERR
    from fno.skill_diff import synthesize as s
    monkeypatch.setattr(cli, "_file_no_diff_node", lambda *a, **k: "fno-y")
    monkeypatch.setattr(cli.synthesize, "synthesize", lambda *a, **k: s.Proposal(
        verdict="propose_pr",
        hunks=[{"file": "f", "old_text": "", "new_text": "x", "cited_finding_ids": [], "rationale": ""}],
    ))
    p = _wire(monkeypatch, tmp_path, [_rc("r1"), _finding("r1")], level="assisted")
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert "no-diff-helps" in r.output and "uncited" in r.output
    ndh = [e for e in _events(p) if e["type"] == "skill_diff_no_diff_helps"]
    assert ndh[0]["data"]["reason"] == "all_hunks_uncited"


def test_no_diff_helps_defers_when_node_filing_fails(monkeypatch, tmp_path):  # P1 review
    # Filing the backlog node fails -> NO terminal event, so the next tick retries.
    monkeypatch.setattr(cli, "_file_no_diff_node", lambda *a, **k: None)
    events = [_rc("r1"), _rc("r2"), _rc("r3"), _finding("r3"),
              {"type": "skill_diff_proposed", "data": {"run_id": "r1", "skill_id": "fno:blueprint"}},
              {"type": "skill_diff_proposed", "data": {"run_id": "r2", "skill_id": "fno:blueprint"}}]
    p = _wire(monkeypatch, tmp_path, events, level="assisted")
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert "will retry" in r.output
    assert "skill_diff_no_diff_helps" not in [e["type"] for e in _events(p)]


def test_transient_open_failure_is_not_terminal(monkeypatch, tmp_path):  # P2 review
    from fno.skill_diff import synthesize as s
    monkeypatch.setattr(cli.synthesize, "synthesize", lambda *a, **k: s.Proposal(
        verdict="propose_pr",
        hunks=[{"file": "skills/blueprint/SKILL.md", "old_text": "", "new_text": "x",
                "cited_finding_ids": ["s1"], "rationale": "r"}]))

    def boom(**k):
        raise RuntimeError("git push rejected")

    monkeypatch.setattr(cli, "_apply_and_open_pr", boom)
    p = _wire(monkeypatch, tmp_path, [_rc("r1"), _finding("r1")], level="assisted")
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert "open-failed" in r.output
    types = [e["type"] for e in _events(p)]
    assert "skill_diff_no_diff_helps" not in types and "skill_diff_proposed" not in types


def test_redaction_refusal_is_terminal(monkeypatch, tmp_path):  # P2 review
    from fno.skill_diff import synthesize as s
    monkeypatch.setattr(cli.synthesize, "synthesize", lambda *a, **k: s.Proposal(
        verdict="propose_pr",
        hunks=[{"file": "skills/blueprint/SKILL.md", "old_text": "", "new_text": "x",
                "cited_finding_ids": ["s1"], "rationale": "r"}]))

    def refuse(**k):
        raise cli.RedactionRefused("leak")

    monkeypatch.setattr(cli, "_apply_and_open_pr", refuse)
    p = _wire(monkeypatch, tmp_path, [_rc("r1"), _finding("r1")], level="assisted")
    r = runner.invoke(cli.skill_diff_app, ["tick", "--skill", "blueprint"])
    assert "redaction refused" in r.output
    ndh = [e for e in _events(p) if e["type"] == "skill_diff_no_diff_helps"]
    assert ndh and ndh[0]["data"]["reason"] == "redaction_refused"


def test_apply_refuses_path_traversal(monkeypatch, tmp_path):
    # An LLM-supplied path that escapes the target skill dir must be refused.
    with pytest.raises(RuntimeError, match="not a .md under"):
        cli._apply_and_open_pr(
            skill_id="fno:blueprint", run_id="obs-r1",
            hunks=[{"file": "../../etc/passwd", "old_text": "", "new_text": "x",
                    "cited_finding_ids": ["s1"]}],
            body="b", cited=["s1"],
        )


def test_apply_refuses_non_markdown_and_other_skill(monkeypatch, tmp_path):
    # A .py path, or a path under a DIFFERENT skill, is refused (P2 review).
    for bad in ("skills/blueprint/scripts/x.py", "skills/review/SKILL.md"):
        with pytest.raises(RuntimeError, match="not a .md under"):
            cli._apply_and_open_pr(
                skill_id="fno:blueprint", run_id="obs-r1",
                hunks=[{"file": bad, "old_text": "", "new_text": "x", "cited_finding_ids": ["s1"]}],
                body="b", cited=["s1"],
            )


def test_apply_refuses_redaction_in_hunk_text(monkeypatch, tmp_path):
    # A leak in the committed hunk content (not just PR body) is refused (P1 review).
    monkeypatch.setattr(cli, "_project_names", lambda: [])
    with pytest.raises(cli.RedactionRefused):
        cli._apply_and_open_pr(
            skill_id="fno:blueprint", run_id="obs-r1",
            hunks=[{"file": "skills/blueprint/SKILL.md", "old_text": "",
                    "new_text": "see internal/fno/secret.md", "cited_finding_ids": ["s1"]}],
            body="clean body", cited=["s1"],
        )


def test_registered_in_top_level_cli():
    # The lazy registry must map `skill-diff` to this app, else the verb is
    # unreachable from `fno` even though the module imports fine.
    from fno.cli import LAZY_SUBCOMMANDS

    assert LAZY_SUBCOMMANDS["skill-diff"][0] == "fno.skill_diff.cli:skill_diff_app"


# --------------------------------------------------------------------------- #
# reconcile: detect-and-run (x-ed13)
# --------------------------------------------------------------------------- #

def _find(run_id, corpus_item, verdict="fail", dim="structural_validity", tool_fault=False):
    d = {"run_id": run_id, "skill_id": "fno:blueprint", "corpus_item_id": corpus_item,
         "dimension": dim, "verdict": verdict}
    if tool_fault:
        d["tool_fault"] = True
    return {"type": "skill_eval_finding", "data": d}


def _proposed(pr, run_id="r1", skill_id="fno:blueprint"):
    return {"type": "skill_diff_proposed",
            "data": {"run_id": run_id, "skill_id": skill_id, "pr_number": pr}}


def _wire_reeval(monkeypatch, path, merged=True, merge_sha="mergesha12345", replay=None):
    """Inject the reconcile re-eval seams. ``replay`` maps corpus_item -> the
    after-verdict the simulated observer emits ('pass'/'fail'/'tool_fault'/None).
    Appends the after-finding to the events file so reconcile's re-read sees it."""
    monkeypatch.setattr(cli, "_pr_merged", lambda pr: merged)
    monkeypatch.setattr(cli, "_merge_sha", lambda pr: merge_sha)
    calls = []

    def fake_replay(corpus_item, skill_ref, run_id_after):
        calls.append((corpus_item, skill_ref, run_id_after))
        verdict = (replay or {}).get(corpus_item)
        if verdict is None:
            return 1  # observer emitted nothing usable (batch failure for this item)
        tf = verdict == "tool_fault"
        with path.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(_find(
                run_id_after, corpus_item,
                verdict="fail" if tf else verdict, tool_fault=tf)) + "\n")
        return 0

    monkeypatch.setattr(cli, "_run_replay", fake_replay)
    return calls


def test_reconcile_ac1_hp_structural_receipt(monkeypatch, tmp_path):  # AC1-HP
    events = [_rc("r1"), _find("r1", "sid-a"), _find("r1", "sid-b"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    _wire_reeval(monkeypatch, p, replay={"sid-a": "pass", "sid-b": "pass"})
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert r.exit_code == 0 and "re-evaluated: delta=2" in r.output
    closed = [e for e in _events(p) if e["type"] == "skill_diff_eval_closed"]
    assert closed and closed[0]["data"]["score_delta"] == 2
    assert closed[0]["data"]["run_id_before"] == "r1"
    assert closed[0]["data"]["run_id_after"]  # a real after-run id, not null


def test_reconcile_ac2_err_tool_fault_excluded(monkeypatch, tmp_path):  # AC2-ERR
    events = [_rc("r1"), _find("r1", "sid-a"), _find("r1", "sid-b"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    # sid-a's replay tool-faults (excluded); sid-b genuinely still fails.
    _wire_reeval(monkeypatch, p, replay={"sid-a": "tool_fault", "sid-b": "fail"})
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    closed = [e for e in _events(p) if e["type"] == "skill_diff_eval_closed"]
    # before_fail=2, after_fail=1 (tool_fault not counted) -> delta=1, not 0.
    assert closed and closed[0]["data"]["score_delta"] == 1


def test_reconcile_ac3_err_batch_failure_leaves_unclosed(monkeypatch, tmp_path):  # AC3-ERR
    events = [_rc("r1"), _find("r1", "sid-a"), _find("r1", "sid-b"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    # Every replay tool-faults -> no comparable top-dim verdict -> no receipt.
    _wire_reeval(monkeypatch, p, replay={"sid-a": "tool_fault", "sid-b": "tool_fault"})
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "left unclosed for retry" in r.output
    assert "skill_diff_eval_closed" not in [e["type"] for e in _events(p)]


def test_reconcile_non_replayable_top_dim_is_outcome_pending(monkeypatch, tmp_path):  # codex P1
    # shipped_outcome is the top failing dim, but replay is structural-only and
    # never re-scores it -> must NOT replay and must NOT report a fabricated
    # positive delta; close as outcome-pending (null delta, no replay).
    events = [_rc("r1", top="shipped_outcome"),
              _find("r1", "sid-a", dim="shipped_outcome"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    calls = _wire_reeval(monkeypatch, p, replay={"sid-a": "pass"})
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "outcome-pending" in r.output and "not structurally replayable" in r.output
    assert not calls  # nothing replayed (no spend)
    closed = [e for e in _events(p) if e["type"] == "skill_diff_eval_closed"]
    assert closed and closed[0]["data"]["score_delta"] is None
    assert closed[0]["data"]["run_id_after"] is None


def test_reconcile_receipt_emit_failure_leaves_unclosed(monkeypatch, tmp_path):  # codex P2
    events = [_rc("r1"), _find("r1", "sid-a"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    _wire_reeval(monkeypatch, p, replay={"sid-a": "pass"})
    # Canonical-log append fails -> _emit returns False -> report unclosed, never
    # claim a close we did not durably record.
    monkeypatch.setattr(cli, "_emit", lambda *a, **k: False)
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "receipt emit failed" in r.output and "left unclosed for retry" in r.output


def test_reconcile_concurrent_close_is_noop(monkeypatch, tmp_path):  # codex P1 (race guard)
    events = [_rc("r1"), _find("r1", "sid-a"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    monkeypatch.setattr(cli, "_pr_merged", lambda pr: True)
    monkeypatch.setattr(cli, "_merge_sha", lambda pr: "mergesha12345")

    def racing_replay(corpus_item, skill_ref, run_id_after):
        # Simulate a concurrent reconcile closing the PR during our replay window,
        # plus our own after-finding.
        with p.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(_find(run_id_after, corpus_item, verdict="pass")) + "\n")
            fh.write(json.dumps({"type": "skill_diff_eval_closed",
                     "data": {"pr_number": 201, "skill_id": "fno:blueprint",
                              "run_id_before": "r1"}}) + "\n")
        return 0

    monkeypatch.setattr(cli, "_run_replay", racing_replay)
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "already closed by a concurrent reconcile" in r.output
    # Exactly one receipt (the concurrent one) - we did not append a second.
    assert [e["type"] for e in _events(p)].count("skill_diff_eval_closed") == 1


def test_reconcile_ac5_edge_zero_failure_null_after(monkeypatch, tmp_path):  # AC5-EDGE
    # A merged proposer PR whose before run has no failing items on the top dim.
    events = [_rc("r1", top="structural_validity"), _find("r1", "sid-a", verdict="pass"),
              _proposed(201)]
    # Blank the ranking so top_dimension() is None (no failing dimension).
    events[0]["data"]["failure_ranking"] = []
    p = _wire(monkeypatch, tmp_path, events)
    calls = _wire_reeval(monkeypatch, p)
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "delta=0" in r.output and not calls  # nothing replayed
    closed = [e for e in _events(p) if e["type"] == "skill_diff_eval_closed"]
    assert closed and closed[0]["data"]["score_delta"] == 0
    assert closed[0]["data"]["run_id_after"] is None


def test_reconcile_ac8_fr_paused_skips_cleanly(monkeypatch, tmp_path):  # AC8-FR
    events = [_rc("r1"), _find("r1", "sid-a"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events, paused=True)
    calls = _wire_reeval(monkeypatch, p, replay={"sid-a": "pass"})
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "paused" in r.output and not calls
    assert "skill_diff_eval_closed" not in [e["type"] for e in _events(p)]


def test_reconcile_review_skill_logs_and_skips(monkeypatch, tmp_path):
    events = [_proposed(202, skill_id="fno:review")]
    p = _wire(monkeypatch, tmp_path, events)
    _wire_reeval(monkeypatch, p)
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "202"])
    assert "outcome-pending (review-skill)" in r.output
    assert "skill_diff_eval_closed" not in [e["type"] for e in _events(p)]


def test_reconcile_not_yet_merged(monkeypatch, tmp_path):
    events = [_rc("r1"), _find("r1", "sid-a"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    _wire_reeval(monkeypatch, p, merged=False)
    r = runner.invoke(cli.skill_diff_app, ["reconcile"])
    assert "not-yet-merged" in r.output
    assert "skill_diff_eval_closed" not in [e["type"] for e in _events(p)]


def test_reconcile_gh_offline_leaves_unclosed(monkeypatch, tmp_path):  # AC3-ERR (None != False)
    # gh unreachable -> _pr_merged is None -> skip-and-retry, NOT re-eval against
    # origin/main (which would score a diff that may never have landed).
    events = [_rc("r1"), _find("r1", "sid-a"), _proposed(201)]
    p = _wire(monkeypatch, tmp_path, events)
    calls = _wire_reeval(monkeypatch, p, merged=None, replay={"sid-a": "pass"})
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "merge status unknown" in r.output and not calls
    assert "skill_diff_eval_closed" not in [e["type"] for e in _events(p)]


def test_merge_sha_treats_literal_null_as_unrecoverable(monkeypatch, tmp_path):
    # `gh ... -q .mergeCommit.oid` prints the string "null" for a null field; it
    # must not be handed to git as a ref (caller falls back to origin/main).
    class _P:
        returncode = 0
        stdout = "null\n"
    monkeypatch.setattr(cli.paths, "resolve_repo_root", lambda: tmp_path)
    monkeypatch.setattr(cli.subprocess, "run", lambda *a, **k: _P())
    assert cli._merge_sha(201) is None


def test_reconcile_pr_number_already_closed_is_noop(monkeypatch, tmp_path):  # AC7-FR
    events = [
        _proposed(201),
        {"type": "skill_diff_eval_closed",
         "data": {"pr_number": 201, "skill_id": "fno:blueprint", "run_id_before": "r1"}},
    ]
    p = _wire(monkeypatch, tmp_path, events)
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "201"])
    assert "already has an eval-closed receipt" in r.output


def test_reconcile_pr_number_unknown(monkeypatch, tmp_path):
    _wire(monkeypatch, tmp_path, [_proposed(201)])
    r = runner.invoke(cli.skill_diff_app, ["reconcile", "--pr-number", "999"])
    assert "not a known proposer PR" in r.output


def test_reconcile_silent_when_closed(monkeypatch, tmp_path):
    events = [
        _proposed(201),
        {"type": "skill_diff_eval_closed", "data": {"pr_number": 201, "skill_id": "fno:blueprint", "run_id_before": "r1"}},
    ]
    _wire(monkeypatch, tmp_path, events)
    r = runner.invoke(cli.skill_diff_app, ["reconcile"])
    assert "no un-closed" in r.output
