"""`fno backlog triage consistency` (x-64cb US4).

AC4-HP: K runs over one frozen context report per-category agreement and list
the node ids whose proposed priority differed. AC7-FR: an errored run is counted
and agreement is computed over the completed runs only. Plus the boundary guards
(--repeat < 1, the K>10 cost gate, empty-context short-circuit).

The headless dispatch is monkeypatched so no real model is called.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

import fno.graph.triage as triage
from fno.cli import app
from fno.graph.triage import fold_consistency

runner = CliRunner()


def _prop(priority_changes):
    return {"priority_changes": priority_changes, "dependencies": [], "defer": [], "duplicates": []}


# --- pure fold (AC4-HP core) ---

def test_fold_flags_priority_disagreement():
    runs = [
        _prop([{"id": "ab-1", "to": "p1"}, {"id": "ab-2", "to": "p2"}]),
        _prop([{"id": "ab-1", "to": "p1"}, {"id": "ab-2", "to": "p1"}]),  # ab-2 differs
        _prop([{"id": "ab-1", "to": "p1"}, {"id": "ab-2", "to": "p2"}]),
    ]
    ag = fold_consistency(runs)["priority"]
    assert ag["total"] == 2
    assert ag["agree"] == 1  # ab-1 agrees
    assert ag["disagreeing"] == ["ab-2"]


def test_fold_omission_is_disagreement():
    # A run that omits a node it proposes elsewhere disagrees (None != "p1").
    runs = [_prop([{"id": "ab-1", "to": "p1"}]), _prop([])]
    ag = fold_consistency(runs)["priority"]
    assert ag["disagreeing"] == ["ab-1"]


# --- verb: end-to-end with a stubbed dispatch ---

@pytest.fixture()
def frozen(tmp_path):
    ctx = {"candidates": [{"id": "ab-1", "title": "X", "priority": "p2"}], "ideas": []}
    p = tmp_path / "ctx.json"
    p.write_text(json.dumps(ctx))
    return p


def test_consistency_reports_disagreement_end_to_end(frozen, monkeypatch):
    seq = iter([
        _prop([{"id": "ab-1", "to": "p1"}]),
        _prop([{"id": "ab-1", "to": "p0"}]),  # differs
        _prop([{"id": "ab-1", "to": "p1"}]),
    ])
    monkeypatch.setattr(triage, "_run_consistency_propose", lambda ctx, model: next(seq))
    r = runner.invoke(app, ["backlog", "triage", "consistency", "--repeat", "3", "--frozen-context", str(frozen)])
    assert r.exit_code == 0, r.output
    assert "3/3 runs completed (0 errored)" in r.output
    assert "priority: 0/1 agree" in r.output
    assert "disagreeing: ab-1" in r.output


def test_consistency_errored_run_computed_over_completed(frozen, monkeypatch):
    calls = {"n": 0}

    def flaky(ctx, model):
        calls["n"] += 1
        if calls["n"] == 2:
            raise RuntimeError("spawn failure")
        return _prop([{"id": "ab-1", "to": "p1"}])

    monkeypatch.setattr(triage, "_run_consistency_propose", flaky)
    r = runner.invoke(app, ["backlog", "triage", "consistency", "--repeat", "3", "--frozen-context", str(frozen)])
    assert r.exit_code == 0, r.output
    assert "2/3 runs completed (1 errored)" in r.output  # AC7-FR
    assert "priority: 1/1 agree" in r.output  # the 2 completed agree


def test_repeat_below_one_rejected(frozen):
    r = runner.invoke(app, ["backlog", "triage", "consistency", "--repeat", "0", "--frozen-context", str(frozen)])
    assert r.exit_code != 0
    assert "must be >= 1" in r.output


def test_repeat_over_ten_requires_yes(frozen, monkeypatch):
    called = {"n": 0}
    monkeypatch.setattr(triage, "_run_consistency_propose", lambda c, m: called.__setitem__("n", called["n"] + 1))
    r = runner.invoke(app, ["backlog", "triage", "consistency", "--repeat", "11", "--frozen-context", str(frozen)])
    assert r.exit_code == 2
    assert called["n"] == 0  # cost guard fired before any dispatch
    assert "--yes" in r.output


def test_empty_context_short_circuits(tmp_path, monkeypatch):
    empty = tmp_path / "empty.json"
    empty.write_text(json.dumps({"candidates": [], "ideas": []}))

    def boom(ctx, model):
        raise AssertionError("must not dispatch with zero candidates")

    monkeypatch.setattr(triage, "_run_consistency_propose", boom)
    r = runner.invoke(app, ["backlog", "triage", "consistency", "--repeat", "3", "--frozen-context", str(empty)])
    assert r.exit_code == 0
    assert "nothing to propose" in r.output


def test_empty_context_skips_high_repeat_guard(tmp_path, monkeypatch):
    # --repeat 11 on an empty context makes zero LLM calls, so it must NOT
    # demand --yes (peer review, PR #285): exit 0, not the exit-2 cost gate.
    empty = tmp_path / "empty.json"
    empty.write_text(json.dumps({"candidates": [], "ideas": []}))
    monkeypatch.setattr(triage, "_run_consistency_propose", lambda c, m: (_ for _ in ()).throw(AssertionError("no dispatch")))
    r = runner.invoke(app, ["backlog", "triage", "consistency", "--repeat", "11", "--frozen-context", str(empty)])
    assert r.exit_code == 0
    assert "nothing to propose" in r.output


def test_k1_notes_it_measures_nothing(frozen, monkeypatch):
    monkeypatch.setattr(triage, "_run_consistency_propose", lambda c, m: _prop([{"id": "ab-1", "to": "p1"}]))
    r = runner.invoke(app, ["backlog", "triage", "consistency", "--repeat", "1", "--frozen-context", str(frozen)])
    assert r.exit_code == 0
    assert "K=1 measures nothing" in r.output
