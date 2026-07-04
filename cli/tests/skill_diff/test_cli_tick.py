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


def test_apply_refuses_path_traversal(monkeypatch, tmp_path):
    # An LLM-supplied path that escapes skills/ must be refused before any write.
    with pytest.raises(RuntimeError, match="escapes skills/"):
        cli._apply_and_open_pr(
            skill_id="fno:blueprint", run_id="obs-r1",
            hunks=[{"file": "../../etc/passwd", "old_text": "", "new_text": "x",
                    "cited_finding_ids": ["s1"]}],
            body="b", cited=["s1"],
        )


def test_reconcile_reports_unclosed_pr(monkeypatch, tmp_path):  # AC10-FR
    monkeypatch.setattr(cli, "_pr_merged", lambda pr: True)
    events = [{"type": "skill_diff_proposed",
               "data": {"run_id": "r1", "skill_id": "fno:blueprint", "pr_number": 201}}]
    _wire(monkeypatch, tmp_path, events)
    r = runner.invoke(cli.skill_diff_app, ["reconcile"])
    assert "PR#201" in r.output and "no eval-closed" in r.output


def test_registered_in_top_level_cli():
    # The lazy registry must map `skill-diff` to this app, else the verb is
    # unreachable from `fno` even though the module imports fine.
    from fno.cli import LAZY_SUBCOMMANDS

    assert LAZY_SUBCOMMANDS["skill-diff"][0] == "fno.skill_diff.cli:skill_diff_app"


def test_reconcile_silent_when_closed(monkeypatch, tmp_path):
    events = [
        {"type": "skill_diff_proposed", "data": {"run_id": "r1", "skill_id": "fno:blueprint", "pr_number": 201}},
        {"type": "skill_diff_eval_closed", "data": {"pr_number": 201, "skill_id": "fno:blueprint", "run_id_before": "r1"}},
    ]
    _wire(monkeypatch, tmp_path, events)
    r = runner.invoke(cli.skill_diff_app, ["reconcile"])
    assert "no un-closed" in r.output
