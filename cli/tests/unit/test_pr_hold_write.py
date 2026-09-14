"""fno do pr hold set/release: the receipted writer for one plan's dispatch_hold."""

from __future__ import annotations

import json

import pytest

from fno.graph.ladder import DispatchHoldState, dispatch_hold
from fno.pr import _hold

PLAN = "---\nclaims: t-0001\nstatus: ready\nkind: quick-plan\npriority: p1\n---\n\n# A plan\n\nBody.\n"


@pytest.fixture
def node_plan(tmp_path):
    plan = tmp_path / "plan.md"
    plan.write_text(PLAN, encoding="utf-8")
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "t-0001",
                        "slug": "a-plan",
                        "plan_path": str(plan),
                        "cwd": str(tmp_path),
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    return plan, str(graph)


def _set(graph, **kw):
    base = dict(reason="condition R", release_when="when W", set_by="crown", graph_path=graph)
    base.update(kw)
    return _hold.hold_write("set", "t-0001", **base)


def _release(graph, **kw):
    base = dict(evidence="condition held", graph_path=graph)
    base.update(kw)
    return _hold.hold_write("release", "t-0001", **base)


def _reader(plan):
    return dispatch_hold({"plan_path": str(plan), "cwd": str(plan.parent)})


def test_set_writes_a_block_the_merge_gate_reads(node_plan):
    plan, graph = node_plan
    receipt = _set(graph)
    assert receipt["action"] == "set"
    assert receipt["hold"]["reason"] == "condition R"
    assert _reader(plan).state is DispatchHoldState.HELD
    # Every other byte is preserved: dropping the block restores the original.
    assert _hold._remove_hold_block(plan.read_text(encoding="utf-8")) == PLAN


def test_set_refusals_write_nothing(node_plan):
    plan, graph = node_plan
    for kw in (dict(node="t-nope"), dict(reason="  "), dict(review_on="2026-13-99")):
        node = kw.pop("node", "t-0001")
        with pytest.raises(_hold.HoldWriteError) as err:
            _hold.hold_write("set", node, graph_path=graph, **kw)
        assert err.value.exit_code == 2
    assert plan.read_text(encoding="utf-8") == PLAN


def test_set_refuses_an_already_held_plan(node_plan):
    plan, graph = node_plan
    _set(graph)
    with pytest.raises(_hold.HoldWriteError) as err:
        _set(graph)
    assert err.value.exit_code == 3
    assert "condition R" in str(err.value)
    assert "hold release" in str(err.value)
    assert plan.read_text(encoding="utf-8").count("dispatch_hold:") == 1


def test_release_lifts_and_restores_the_original_bytes(node_plan):
    plan, graph = node_plan
    _set(graph)
    receipt = _release(graph)
    assert receipt["action"] == "release"
    assert receipt["hold"]["evidence"] == "condition held"
    assert receipt["hold"]["still_held_by"] is None
    assert _reader(plan).state is DispatchHoldState.ABSENT
    assert plan.read_text(encoding="utf-8") == PLAN


def test_release_refusals(node_plan):
    plan, graph = node_plan
    with pytest.raises(_hold.HoldWriteError) as err:
        _release(graph)
    assert err.value.exit_code == 3
    assert plan.read_text(encoding="utf-8") == PLAN
    _set(graph)
    with pytest.raises(_hold.HoldWriteError) as err:
        _release(graph, evidence="")
    assert err.value.exit_code == 2


def test_a_failed_readback_restores_the_original_bytes(node_plan, tmp_path):
    plan, graph = node_plan
    entry = {"id": "t-0001", "plan_path": str(plan), "cwd": str(tmp_path)}
    with pytest.raises(_hold.HoldWriteError) as err:
        _hold._write_proven(PLAN + "dispatch_hold: 42\n", str(plan), entry, True, PLAN)
    assert err.value.exit_code == 1
    assert "restored" in str(err.value)
    assert plan.read_text(encoding="utf-8") == PLAN


def test_release_names_an_ancestor_that_still_holds(tmp_path):
    parent_plan = tmp_path / "parent.md"
    parent_plan.write_text(PLAN, encoding="utf-8")
    child_plan = tmp_path / "child.md"
    child_plan.write_text(PLAN, encoding="utf-8")
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "t-parent", "slug": "parent", "plan_path": str(parent_plan), "cwd": str(tmp_path)},
                    {"id": "t-0001", "slug": "a-plan", "parent": "t-parent", "plan_path": str(child_plan), "cwd": str(tmp_path)},
                ]
            }
        ),
        encoding="utf-8",
    )
    _hold.hold_write("set", "t-parent", reason="R", release_when="W", set_by="crown", graph_path=str(graph))
    _hold.hold_write("set", "t-0001", reason="R", release_when="W", set_by="crown", graph_path=str(graph))
    receipt = _hold.hold_write("release", "t-0001", evidence="proved", graph_path=str(graph))
    assert receipt["hold"]["still_held_by"] == "dispatch-hold:t-parent"


def test_default_review_on_is_today_plus_seven(node_plan):
    import datetime

    plan, graph = node_plan
    receipt = _set(graph)
    expected = (datetime.date.today() + datetime.timedelta(days=7)).isoformat()
    assert receipt["hold"]["review_on"] == expected
    assert plan.read_text(encoding="utf-8").count("dispatch_hold:") == 1


def test_set_disarms_an_armed_auto_merge(tmp_path, monkeypatch):
    plan = tmp_path / "plan.md"
    plan.write_text(PLAN, encoding="utf-8")
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "t-0001",
                        "slug": "a-plan",
                        "plan_path": str(plan),
                        "cwd": str(tmp_path),
                        "pr_number": 42,
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    calls: list[int] = []
    monkeypatch.setattr(_hold, "_disarm_queued_auto_merge", lambda pr, cwd, why: calls.append(pr))
    receipt = _hold.hold_write("set", "t-0001", reason="R", release_when="W", set_by="crown", graph_path=str(graph))
    assert calls == [42]
    assert receipt["disarm"] == "issued"


def test_cli_hold_set_and_release_round_trip(node_plan, monkeypatch):
    from pathlib import Path

    from typer.testing import CliRunner

    from fno.cli import app
    from fno import paths as fno_paths

    plan, graph = node_plan
    monkeypatch.setattr(fno_paths, "graph_json", lambda: Path(graph))
    runner = CliRunner()
    result = runner.invoke(
        app,
        ["do", "pr", "hold", "set", "t-0001", "--reason", "R", "--release-when", "W", "--set-by", "crown"],
    )
    assert result.exit_code == 0, result.output
    assert _reader(plan).state is DispatchHoldState.HELD
    result = runner.invoke(app, ["do", "pr", "hold", "release", "t-0001", "--evidence", "proved"])
    assert result.exit_code == 0, result.output
    assert plan.read_text(encoding="utf-8") == PLAN
