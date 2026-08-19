from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.cli import app
from fno.graph.ladder import DispatchHoldState
from fno.pr import _hold
from fno.pr._proc import Result


def _graph(tmp_path, monkeypatch, *, plan_body: str):
    plan = tmp_path / "held.md"
    plan.write_text(plan_body)
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [{
        "id": "x-5a5c",
        "cwd": str(tmp_path),
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
        "plan_path": str(plan),
    }]}))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr(
        "fno.pr._proc.run",
        lambda *a, **k: Result(0, "https://github.com/o/r/pull/42\n", ""),
    )
    return graph


def test_hold_for_pr_returns_attributable_plan_hold(tmp_path, monkeypatch):
    _graph(
        tmp_path,
        monkeypatch,
        plan_body=(
            "---\nstatus: ready\ndispatch_hold:\n"
            "  reason: Blocking finding\n  release_when: Finding fixed\n"
            "  review_on: 2099-08-20\n  set_by: king:119e3c52\n---\n"
        ),
    )
    verdict = _hold.hold_for_pr(42, str(tmp_path))
    assert verdict is not None
    assert verdict.owner_id == "x-5a5c"
    assert verdict.hold.state is DispatchHoldState.HELD
    assert verdict.hold.set_by == "king:119e3c52"


def test_hold_for_pr_fails_closed_when_bound_plan_is_unreadable(tmp_path, monkeypatch):
    graph = _graph(tmp_path, monkeypatch, plan_body="---\nstatus: ready\n---\n")
    data = json.loads(graph.read_text())
    data["entries"][0]["plan_path"] = str(tmp_path / "missing.md")
    graph.write_text(json.dumps(data))
    reason = _hold.merge_hold_reason(42, str(tmp_path))
    assert reason is not None and "dispatch-hold-invalid:x-5a5c" in reason


def test_hold_check_cli_refuses_with_reason_and_setter(tmp_path, monkeypatch):
    _graph(
        tmp_path,
        monkeypatch,
        plan_body=(
            "---\nstatus: ready\ndispatch_hold:\n"
            "  reason: Blocking finding\n  release_when: Finding fixed\n"
            "  review_on: 2099-08-20\n  set_by: king:119e3c52\n---\n"
        ),
    )
    result = CliRunner().invoke(app, ["pr", "hold-check", "42", "--repo", str(tmp_path)])
    assert result.exit_code == 3, result.output
    assert "dispatch-hold:x-5a5c" in result.output
    assert "set_by=king:119e3c52" in result.output
