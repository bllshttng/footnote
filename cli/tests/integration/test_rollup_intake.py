"""Integration: the rollup ladder fires on the `idea`/`add` intake path.

Covers AC1 (auto-link + receipt), AC2 (suggest below the bar), the orphan line,
and AC4 (a rollup failure never breaks intake).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.graph._constants as gc
import fno.graph.store as gs
from fno.cli import app

runner = CliRunner()


def _invoke(*args):
    return runner.invoke(app, list(args), catch_exceptions=False)


def _route_graph(g: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)


def _epic(nid: str, title: str) -> dict:
    return {
        "id": nid, "parent": None, "title": title, "type": "epic",
        "project": "fno", "cwd": "/tmp/proj", "priority": "p1", "domain": "code",
        "blocked_by": [], "created_at": "2026-01-01T00:00:00+00:00",
    }


@pytest.fixture
def graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    g = tmp_path / "graph.json"

    def _write(entries: list[dict]) -> Path:
        g.write_text(json.dumps({"entries": entries}), encoding="utf-8")
        _route_graph(g, tmp_path, monkeypatch)
        return g

    return _write


def _nodes(g: Path) -> list[dict]:
    return json.loads(g.read_text(encoding="utf-8"))["entries"]


def _created(g: Path, title: str) -> dict:
    return next(e for e in _nodes(g) if e.get("title") == title)


def test_auto_link_sets_parent_and_prints_receipt(graph):
    """AC1: a clear match is linked in the same write, with an undo command."""
    g = graph([_epic("x-mux0001", "mux pane layout polish")])
    title = "mux pane layout polish resize"

    res = _invoke("backlog", "idea", title, "--cwd", "/tmp/proj")

    assert res.exit_code == 0
    assert _created(g, title)["parent"] == "x-mux0001"
    assert "rollup: auto-linked" in res.stderr
    assert "x-mux0001" in res.stderr
    assert "--parent null" in res.stderr


def test_suggest_below_the_bar_writes_no_parent(graph):
    """AC2: near-tied epics produce suggestions and no mutation."""
    g = graph([
        _epic("x-aaa00001", "billing invoice export pipeline"),
        _epic("x-bbb00002", "billing invoice export workflow"),
    ])
    title = "billing invoice export"

    res = _invoke("backlog", "idea", title, "--cwd", "/tmp/proj")

    assert res.exit_code == 0
    assert _created(g, title).get("parent") is None
    assert "--parent x-aaa00001" in res.stderr
    assert "--parent x-bbb00002" in res.stderr
    assert "auto-linked" not in res.stderr


def test_no_candidates_prints_the_orphan_hint(graph):
    """AC3: with a live epic present, an unmatched feature is told it is one."""
    g = graph([_epic("x-mux0001", "mux pane layout polish")])
    title = "quantum teapot calibration"

    res = _invoke("backlog", "idea", title, "--cwd", "/tmp/proj")

    assert res.exit_code == 0
    assert _created(g, title).get("parent") is None
    assert "--orphan-ok" in res.stderr


def test_greenfield_graph_is_quiet_and_does_not_crash(graph):
    """No epics means no mission to resolve; intake must not narrate that."""
    g = graph([])
    res = _invoke("backlog", "idea", "first ever node", "--cwd", "/tmp/proj")
    assert res.exit_code == 0
    assert len(_nodes(g)) == 1
    assert "rollup" not in res.stderr


def test_explicit_parent_is_never_second_guessed(graph):
    """A hand-set parent already resolves, so the ladder stays silent."""
    g = graph([
        _epic("x-mux0001", "mux pane layout polish"),
        _epic("x-oth00002", "other mission"),
    ])
    title = "mux pane layout polish resize"

    res = _invoke(
        "backlog", "idea", title, "--cwd", "/tmp/proj", "--parent", "x-oth00002"
    )

    assert _created(g, title)["parent"] == "x-oth00002"
    assert "rollup:" not in res.stderr


def test_bug_type_is_exempt_from_the_ladder(graph):
    """AC6: a bug never gets a rollup line, however well it scores."""
    graph([_epic("x-mux0001", "mux pane layout polish")])
    res = _invoke(
        "backlog", "idea", "mux pane layout polish", "--cwd", "/tmp/proj",
        "--type", "bug",
    )
    assert res.exit_code == 0
    assert "rollup:" not in res.stderr


def test_rollup_failure_never_breaks_intake(graph, monkeypatch):
    """AC4: a raising scorer still files the node, exit 0, one stderr warning."""
    import fno.graph.rollup as rollup

    g = graph([_epic("x-mux0001", "mux pane layout polish")])

    def boom(*a, **k):
        raise RuntimeError("simulated scorer corruption")

    monkeypatch.setattr(rollup, "resolve", boom)
    title = "mux pane layout polish resize"

    res = _invoke("backlog", "idea", title, "--cwd", "/tmp/proj")

    assert res.exit_code == 0
    created = _created(g, title)
    assert created.get("parent") is None
    assert "rollup skipped" in res.stderr
    assert "rollup: auto-linked" not in res.stderr


def test_auto_link_repaints_the_parent_rollup(graph, monkeypatch):
    """The x-6c2b pitfall: an auto-link must repaint children_total."""
    seen: list = []
    import fno.graph.cli as gcli

    graph([_epic("x-mux0001", "mux pane layout polish")])
    monkeypatch.setattr(
        gcli, "_project_plans_from_graph", lambda ids: seen.append(list(ids))
    )

    _invoke("backlog", "idea", "mux pane layout polish resize", "--cwd", "/tmp/proj")

    assert seen, "auto-linked node did not trigger an ancestor repaint"


def test_a_link_never_lands_without_its_receipt(graph, monkeypatch):
    """The undo receipt is the mitigation for a wrong auto-link.

    If receipt rendering fails, the parent edge must NOT be written - a silent
    auto-link is exactly the failure the printed receipt exists to prevent.
    """
    import fno.graph.rollup as rollup

    g = graph([_epic("x-mux0001", "mux pane layout polish")])
    monkeypatch.setattr(
        rollup, "receipt_lines",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("render failed")),
    )
    title = "mux pane layout polish resize"

    res = _invoke("backlog", "idea", title, "--cwd", "/tmp/proj")

    assert res.exit_code == 0
    assert _created(g, title).get("parent") is None
    assert "rollup skipped" in res.stderr


def test_stdout_stays_pure_json_for_machine_callers(graph):
    """Regression: callers do `json.loads(result.output)["id"]`.

    The receipt is advisory human output; putting it on stdout corrupted the
    intake verb's machine-readable payload for every scripted consumer.
    """
    graph([_epic("x-mux0001", "mux pane layout polish")])

    res = _invoke("backlog", "idea", "mux pane layout polish resize", "--cwd", "/tmp/proj")

    payload = json.loads(res.stdout)
    assert payload["title"] == "mux pane layout polish resize"
    assert payload["id"]
    assert "rollup" in res.stderr, "the receipt must still be surfaced, on stderr"
