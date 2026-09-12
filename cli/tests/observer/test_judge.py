"""AC3-*/AC4-*/AC5-* for the blueprint judge (x-9983). Every model touch
goes through a fake spawn; no test reaches the network."""

from __future__ import annotations

import pytest
import yaml
from typer.testing import CliRunner

from fno.observer import judge

runner = CliRunner()


@pytest.fixture(autouse=True)
def _no_context(monkeypatch):
    """Unit tests judge prose only; no rg/fno subprocesses."""
    monkeypatch.setattr(judge, "_gather_context", lambda dimension, plan_text: "")


def _fake_spawn(replies, calls=None):
    """replies maps dimension -> full reply text; rc 0. Records (name, prompt)."""

    def spawn(name, prompt):
        if calls is not None:
            calls.append((name, prompt))
        dimension = name.rsplit("-", 1)[-1]
        return 0, replies.get(dimension, "no reply"), ""

    return spawn


def test_judge_plan_fail_verdict_parsed_with_evidence_cap():  # AC3-HP
    reply = (
        '"The operator reads groom reports each morning" quotes a who.\n'
        "No source ties the cost to anything.\n"
        "VERDICT: fail\n"
    )
    verdict, reason = judge.judge_plan("<plan>", "<node>", "persona", spawn=_fake_spawn({"persona": reply}))
    assert verdict == "fail"
    assert "groom reports" in reason
    assert "VERDICT" not in reason


def test_judge_plan_evidence_cut_to_500():  # AC3-HP
    reply = ("x" * 800 + "\nVERDICT: fail\n")
    verdict, reason = judge.judge_plan("<plan>", "<node>", "persona", spawn=_fake_spawn({"persona": reply}))
    assert verdict == "fail"
    assert len(reason) == 500


def test_judge_plan_unknown_and_faults_are_coverage_gaps():  # AC3-ERR
    base = {"persona": "reasoning\nVERDICT: unknown\n"}
    assert judge.judge_plan("<plan>", "<node>", "persona", spawn=_fake_spawn(base)) == (None, "reasoning")

    def rc1(name, prompt):
        return 1, "", "spawn died"

    assert judge.judge_plan("<plan>", "<node>", "persona", spawn=rc1)[0] is None

    def unparseable(name, prompt):
        return 0, "a reply with no verdict line", ""

    assert judge.judge_plan("<plan>", "<node>", "persona", spawn=unparseable)[0] is None

    def boom(name, prompt):
        raise OSError("timeout")

    verdict, reason = judge.judge_plan("<plan>", "<node>", "persona", spawn=boom)
    assert verdict is None and "spawn fault" in reason


def test_judge_plan_unknown_dimension_is_gap():
    assert judge.judge_plan("<plan>", "<node>", "shipped_outcome", spawn=_fake_spawn({}))[0] is None


def test_prompt_carries_node_plan_and_lens(monkeypatch):
    calls = []
    monkeypatch.setattr(
        judge, "load_lenses", lambda: ("grade one question only", {"persona": "Names a who and a sourced cost."})
    )
    judge.judge_plan(
        "plan body", "node body", "persona", spawn=_fake_spawn({}, calls)
    )
    name, prompt = calls[0]
    assert name == "blueprint-judge-persona"
    assert "node body" in prompt and "plan body" in prompt
    # the lens section comes last, after the shared instruction
    assert prompt.index("grade one question only") < prompt.index("Names a who")


# --------------------------------------------------------------------------- #
# Calibration tally (AC5-*)
# --------------------------------------------------------------------------- #

_LABELS = [
    {
        "plan": "controls/good-plan.md",
        "node_text": "n",
        "split": "test",
        "control": True,
        "labels": {"persona": "pass", "surface_fit": "pass"},
        "note": "",
    },
    {
        "plan": "controls/empty-answers.md",
        "node_text": "n",
        "split": "test",
        "control": True,
        "labels": {"persona": "fail"},
        "note": "",
    },
]


def _plan_loader(rows):
    return {row["plan"]: f"plan for {row['plan']}" for row in rows}


def test_tally_rates_and_disagreements():
    # A judge that passes everything: the fail-labeled control disagrees.
    def passing_spawn(name, prompt):
        return 0, "fine\nVERDICT: pass\n", ""

    out = judge.tally(_LABELS, plan_text=_plan_loader(_LABELS), spawn=passing_spawn)
    persona = out["dimensions"]["persona"]
    assert persona["n"] == 2
    assert persona["tp_rate"] == 0  # judged pass, labeled fail
    assert persona["tn_rate"] == 0.5  # one of two persona pairs labeled pass
    assert len(out["disagreements"]) == 1
    assert out["disagreements"][0]["label"] == "fail"
    assert out["disagreements"][0]["judge"] == "pass"
    assert out["controls_wrong"] == 1


def test_tally_counts_none_as_wrong_on_controls():
    def gap_spawn(name, prompt):
        return 0, "cannot tell\nVERDICT: unknown\n", ""

    out = judge.tally(_LABELS, plan_text=_plan_loader(_LABELS), spawn=gap_spawn)
    assert out["controls_wrong"] == 3  # three labeled pairs, every one a gap


def test_labels_yaml_controls_are_wellformed():
    rows = yaml.safe_load(
        (judge._lenses_path().parent / "labels.yaml").read_text(encoding="utf-8")
    )
    assert len(rows) == 5 and all(r["control"] for r in rows)
    dims = {d for r in rows for d in r["labels"]}
    assert dims <= set(judge.JUDGE_DIMENSIONS)


# --------------------------------------------------------------------------- #
# the judge verb (AC4-*, AC5-HP)
# --------------------------------------------------------------------------- #

def _wire_verb_spawn(monkeypatch, replies=None, calls=None):
    from fno.observer import cli as obs_cli

    def spawn(name, prompt):
        if calls is not None:
            calls.append(name)
        return 0, (replies or {}).get(name, "fine\nVERDICT: pass\n"), ""

    monkeypatch.setattr(obs_cli, "_judge_spawn", lambda: spawn)
    return obs_cli


def test_judge_verb_skips_at_report_level(monkeypatch, tmp_path):  # AC4-HP
    p = tmp_path / "p.md"
    p.write_text("---\ntitle: t\n---\n\n## Five questions\n\n1. persona: the operator\n")
    calls = []
    obs_cli = _wire_verb_spawn(monkeypatch, calls=calls)
    r = runner.invoke(obs_cli.observer_app, ["judge", "--plan", str(p)])
    assert r.exit_code == 0, r.output
    assert "skipped level=report" in r.output
    assert not calls  # no spawn call at report level


def test_judge_verb_unanswered_without_section(monkeypatch, tmp_path):  # AC4-ERR
    p = tmp_path / "p.md"
    p.write_text("---\ntitle: t\n---\n\n## Context\n\nbody\n")
    calls = []
    obs_cli = _wire_verb_spawn(monkeypatch, calls=calls)
    r = runner.invoke(obs_cli.observer_app, ["judge", "--plan", str(p), "--force"])
    assert r.exit_code == 0, r.output
    assert "unanswered" in r.output
    assert not calls  # coverage gap, no model call


def test_judge_labels_calibration_prints_rates_and_exits_1(monkeypatch):  # AC5-HP
    # A pass-everything judge: the four fail-labeled controls are wrong.
    labels = judge._lenses_path().parent / "labels.yaml"
    obs_cli = _wire_verb_spawn(monkeypatch)
    r = runner.invoke(obs_cli.observer_app, ["judge", "--labels", str(labels), "--split", "test"])
    assert r.exit_code == 1, r.output
    assert "tp_rate" in r.output and "tn_rate" in r.output
    assert "controls wrong" in r.output
    # every control disagreement is listed, one line each
    assert r.output.count("label=fail") == 4
