"""AC4-*/AC5-* for the blueprint judge CLI wrapper (x-9983). The grading
itself (lens prompts, model spawn, verdict parsing, calibration tally) is
Rust now: ``crates/fno-agents/src/blueprint_judge.rs``, tested there. This
file tests only the Python seam: the level=report gate, the has-five-questions
gate, and forwarding a ``fno-agents judge`` answer into findings/output -
each with ``_judge_via_rust`` stubbed so no test reaches a real subprocess."""

from __future__ import annotations

import yaml
from typer.testing import CliRunner

from fno.observer import fold

runner = CliRunner()


def _wire(monkeypatch, out):
    from fno.observer import cli as obs_cli

    calls: list[list[str]] = []

    def fake(argv):
        calls.append(argv)
        return out

    monkeypatch.setattr(obs_cli, "_judge_via_rust", fake)
    return obs_cli, calls


def test_judge_verb_skips_at_report_level(monkeypatch, tmp_path):  # AC4-HP
    p = tmp_path / "p.md"
    p.write_text("---\ntitle: t\n---\n\n## Five questions\n\n1. persona: the operator\n")
    obs_cli, calls = _wire(monkeypatch, {"rows": []})
    r = runner.invoke(obs_cli.observer_app, ["judge", "--plan", str(p)])
    assert r.exit_code == 0, r.output
    assert "skipped level=report" in r.output
    assert not calls  # no fno-agents call at report level


def test_judge_verb_unanswered_without_section(monkeypatch, tmp_path):  # AC4-ERR
    p = tmp_path / "p.md"
    p.write_text("---\ntitle: t\n---\n\n## Context\n\nbody\n")
    obs_cli, calls = _wire(monkeypatch, {"rows": []})
    r = runner.invoke(obs_cli.observer_app, ["judge", "--plan", str(p), "--force"])
    assert r.exit_code == 0, r.output
    assert "unanswered" in r.output
    assert not calls  # coverage gap, no fno-agents call


def test_judge_verb_forwards_rows_and_emits_findings(monkeypatch, tmp_path):  # AC4-HP
    p = tmp_path / "p.md"
    p.write_text("---\ntitle: t\n---\n\n## Five questions\n\n1. persona: the operator\n")
    out = {
        "rows": [
            {"dimension": "persona", "verdict": "fail", "reason": "no source"},
            {"dimension": "surface_fit", "verdict": None, "reason": "unanswered"},
        ]
    }
    obs_cli, calls = _wire(monkeypatch, out)
    r = runner.invoke(
        obs_cli.observer_app, ["judge", "--plan", str(p), "--force", "--node", "x-1"]
    )
    assert r.exit_code == 0, r.output
    assert calls and calls[0] == ["--plan", str(p), "--node", "x-1"]
    assert "persona: fail" in r.output
    assert "surface_fit: unanswered" in r.output
    assert "revise the plan once" in r.output


def test_judge_labels_calibration_prints_rates_and_exits_1(monkeypatch):  # AC5-HP
    # Four fail-labeled controls disagreeing with a pass-everything judge.
    out = {
        "dimensions": {"persona": {"n": 4, "tp_rate": 0.0, "tn_rate": 0.25}},
        "disagreements": [
            {
                "plan": p,
                "dimension": "persona",
                "label": "fail",
                "judge": "pass",
                "reason": "r",
            }
            for p in (
                "controls/empty-answers.md",
                "controls/invented-persona.md",
                "controls/wrong-node.md",
            )
        ]
        + [
            {
                "plan": "controls/all-none.md",
                "dimension": "surface_fit",
                "label": "fail",
                "judge": "pass",
                "reason": "r",
            }
        ],
        "controls_wrong": 4,
    }
    obs_cli, calls = _wire(monkeypatch, out)
    r = runner.invoke(
        obs_cli.observer_app, ["judge", "--labels", "labels.yaml", "--split", "test"]
    )
    assert r.exit_code == 1, r.output
    assert calls == [["--labels", "labels.yaml", "--split", "test"]]
    assert "tp_rate" in r.output and "tn_rate" in r.output
    assert "controls wrong" in r.output
    assert r.output.count("label=fail") == 4


def test_labels_yaml_controls_are_wellformed():
    from fno.paths import resolve_repo_root

    rows = yaml.safe_load(
        (resolve_repo_root() / "evals/blueprint-judge/labels.yaml").read_text(encoding="utf-8")
    )
    assert len(rows) == 5 and all(r["control"] for r in rows)
    dims = {d for r in rows for d in r["labels"]}
    assert dims <= set(fold.JUDGE_DIMENSIONS)
