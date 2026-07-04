"""CLI-wiring coverage for `fno observer` (x-57a5, Task 7).

Sweep and replay are exercised with injected seams (fake corpus, fake gh, fake
spawn, no real worktree) so no test touches the network, real ~/.fno state, or
spawns a session. Asserts the anti-silent contract (a state word + the right
events), the insufficient guard (no run_complete), replay's A2 rejection, the
replay tool-fault path, and the one hard failure: an isolation violation.
"""

from __future__ import annotations

import json

import pytest
import typer
from typer.testing import CliRunner

from fno.observer import cli

runner = CliRunner()


def _item(session_id, node_id, plan_path):
    return {
        "session_id": session_id,
        "graph_node_id": node_id,
        "plan_path": str(plan_path) if plan_path else None,
        "skill_id": "fno:blueprint",
        "skill_version": "abc1234",
        "method": "phase-proxy",
        "shipped": True,
        "termination_reason": "DonePRGreen",
        "judgeable": False,
        "outcome": None,
        "attribution_class": None,
    }


def _wire(monkeypatch, tmp_path, items):
    """Redirect events + digest into tmp; inject the corpus."""
    events_path = tmp_path / "events.jsonl"
    monkeypatch.setattr(cli, "_events_paths", lambda: [events_path])
    monkeypatch.setattr(cli, "_load_corpus", lambda skill, since: ({"items": items, "attributed": len(items)}, {}))
    import fno.paths as paths
    monkeypatch.setattr(paths, "observer_reports_dir", lambda *a, **k: tmp_path / "reports")
    return events_path


def _events(path):
    if not path.exists():
        return []
    return [json.loads(ln) for ln in path.read_text().splitlines() if ln.strip()]


def _good_plan(tmp_path, name):
    p = tmp_path / name
    p.write_text(
        "# Plan\n\n## Failure Modes\n\nboundaries\n\n"
        "## Execution Strategy\n\n```yaml\ntasks:\n- id: '1'\n  surface: ['a.py']\n```\n"
    )
    return p


# --------------------------------------------------------------------------- #
# help + A2 rejection
# --------------------------------------------------------------------------- #

def test_help_lists_sweep_and_replay():
    r = runner.invoke(cli.observer_app, ["--help"])
    assert r.exit_code == 0
    assert "sweep" in r.output and "replay" in r.output


def test_replay_review_rejected_nonzero():
    r = runner.invoke(cli.observer_app, ["replay", "--skill", "review", "--corpus-item", "s1"])
    assert r.exit_code != 0
    assert "A2" in r.output


# --------------------------------------------------------------------------- #
# sweep
# --------------------------------------------------------------------------- #

def test_sweep_insufficient_emits_no_run_complete(monkeypatch, tmp_path):
    items = [_item(f"s{i}", f"x-{i}", _good_plan(tmp_path, f"p{i}.md")) for i in range(5)]
    events_path = _wire(monkeypatch, tmp_path, items)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint"])
    assert r.exit_code == 0
    assert "insufficient" in r.output and "5" in r.output
    # AC1-UI: nothing to complete -> no terminal event
    assert not any(e["type"] == "skill_eval_run_complete" for e in _events(events_path))


def test_sweep_ok_emits_findings_run_complete_and_digest(monkeypatch, tmp_path):
    items = [_item(f"s{i}", f"x-{i}", _good_plan(tmp_path, f"p{i}.md")) for i in range(10)]
    events_path = _wire(monkeypatch, tmp_path, items)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint"])
    assert r.exit_code == 0, r.output
    assert r.output.startswith("ok:")
    evs = _events(events_path)
    findings = [e for e in evs if e["type"] == "skill_eval_finding"]
    completes = [e for e in evs if e["type"] == "skill_eval_run_complete"]
    assert len(completes) == 1
    assert completes[0]["data"]["coverage_pct"] == 100
    assert findings and all(f["data"]["verdict"] in ("pass", "degraded", "fail") for f in findings)
    # digest written
    assert list((tmp_path / "reports").glob("blueprint-*.md"))


def test_sweep_partial_when_an_item_is_unscorable(monkeypatch, tmp_path):
    items = [_item(f"s{i}", f"x-{i}", _good_plan(tmp_path, f"p{i}.md")) for i in range(9)]
    items.append(_item("s9", "x-9", tmp_path / "missing.md"))  # no plan on disk, no node -> gap
    events_path = _wire(monkeypatch, tmp_path, items)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint", "--json"])
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert payload["state"] == "partial"
    assert payload["coverage_pct"] == 90  # 9 of 10 scored


# --------------------------------------------------------------------------- #
# replay
# --------------------------------------------------------------------------- #

def _wire_replay(monkeypatch, tmp_path, item):
    events_path = _wire(monkeypatch, tmp_path, [item])
    monkeypatch.setattr(cli, "_read_plan_text", lambda *a, **k: "# Doc\n\nbuild a widget\n")
    import fno.claims.core as claims
    monkeypatch.setattr(claims, "acquire_claim", lambda **k: object())
    monkeypatch.setattr(claims, "release_claim", lambda *a, **k: None)
    return events_path


def test_replay_happy_path_emits_tagged_finding(monkeypatch, tmp_path):
    item = _item("s-rep", "x-rep", None)
    events_path = _wire_replay(monkeypatch, tmp_path, item)
    fresh = "## Failure Modes\nx\n\n## Execution Strategy\n```yaml\ntasks:\n- id: '1'\n  surface: ['a.py']\n```\n"
    cli._replay(
        skill="blueprint", corpus_item="s-rep", skill_ref="cand-branch",
        run_id="obs-fno:blueprint-test", since=90,
        spawn=lambda name, prompt, *, cwd, timeout: (0, fresh, ""),
        run_worktree=False,
    )
    evs = _events(events_path)
    findings = [e for e in evs if e["type"] == "skill_eval_finding"]
    assert findings and all(f["data"].get("skill_ref") == "cand-branch" for f in findings)
    # A1: replay never emits shipped_outcome
    assert all(f["data"]["dimension"] != "shipped_outcome" for f in findings)
    completes = [e for e in evs if e["type"] == "skill_eval_run_complete"]
    assert completes and completes[0]["data"].get("skill_ref") == "cand-branch"


def test_replay_spawn_failure_is_tool_fault(monkeypatch, tmp_path):
    item = _item("s-rep", "x-rep", None)
    events_path = _wire_replay(monkeypatch, tmp_path, item)
    with pytest.raises(typer.Exit) as exc:
        cli._replay(
            skill="blueprint", corpus_item="s-rep", skill_ref=None,
            run_id="obs-fno:blueprint-tf", since=90,
            spawn=lambda name, prompt, *, cwd, timeout: (1, "", "boom"),
            run_worktree=False,
        )
    assert exc.value.exit_code == 1
    findings = [e for e in _events(events_path) if e["type"] == "skill_eval_finding"]
    assert findings and "tool-fault" in findings[0]["data"]["evidence"]


def test_replay_isolation_violation_hard_fails(monkeypatch, tmp_path):
    item = _item("s-rep", "x-rep", None)
    _wire_replay(monkeypatch, tmp_path, item)
    monkeypatch.setattr(cli, "_write_workdir_settings", lambda wd: None)

    class _P:
        returncode, stdout, stderr = 0, "", ""

    monkeypatch.setattr(cli.subprocess, "run", lambda *a, **k: _P())
    from fno.observer import isolation
    monkeypatch.setattr(isolation, "collect_eval_session_ids", lambda wd, **k: ({"leak"}, None))
    monkeypatch.setattr(
        isolation, "check_isolation",
        lambda ids, paths: isolation.IsolationResult(
            verdict="violated",
            violations=[isolation.Violation(path=tmp_path / "ledger.json", session_id="leak", line_number=1, detail="x")],
        ),
    )
    fresh = "## Failure Modes\nx\n\n## Execution Strategy\n```yaml\ntasks:\n- id: '1'\n  surface: ['a.py']\n```\n"
    with pytest.raises(typer.Exit) as exc:
        cli._replay(
            skill="blueprint", corpus_item="s-rep", skill_ref=None,
            run_id="obs-fno:blueprint-iso", since=90,
            spawn=lambda name, prompt, *, cwd, timeout: (0, fresh, ""),
            run_worktree=True,
        )
    assert exc.value.exit_code == 3  # the ONE non-advisory failure
