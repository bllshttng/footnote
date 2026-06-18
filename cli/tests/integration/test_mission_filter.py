"""Integration tests for `fno backlog next --mission <id>` (group 3, ab-9fd662c6).

A megatron-dispatched child walk runs `fno-agents loop run --driver megawalk
--mission <id>`, whose MegawalkQueue passes `--mission` through to
`fno backlog next` so the walk works ONLY the mission's nodes and never
drifts into the project's general backlog.
"""
import json

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _add(title, **opts) -> str:
    args = ["graph", "add", title]
    for k, v in opts.items():
        args += [f"--{k}", str(v)]
    r = runner.invoke(app, args, catch_exceptions=False)
    assert r.exit_code == 0, r.output
    return json.loads(r.output)["id"]


def _set_mission(graph_path, node_id, mission_id, wave=1, slug="2026-06-07-test"):
    """Stamp mission metadata on a node in the TEST graph fixture.

    Production nodes get these fields from `fno backlog intake` lifting the
    plan frontmatter written by megatron's dispatch; tests stamp directly.
    """
    data = json.loads(graph_path.read_text())
    for e in data["entries"]:
        if e["id"] == node_id:
            e["mission_id"] = mission_id
            e["mission_wave"] = wave
            e["mission_slug"] = slug
            e["mission_from_msg_id"] = None
    graph_path.write_text(json.dumps(data))


def test_next_mission_filter_scopes_to_mission_nodes(tmp_graph):
    mission_node = _add("Mission work")
    _add("Unrelated work", priority="p0")  # outranks on priority
    _set_mission(tmp_graph, mission_node, "ab-mq0001")

    r = runner.invoke(
        app,
        ["graph", "next", "--mission", "ab-mq0001", "--include-ideas", "--all"],
        catch_exceptions=False,
    )

    assert r.exit_code == 0, r.output
    picked = json.loads(r.output)
    assert picked is not None
    assert picked["id"] == mission_node
    assert picked["mission_id"] == "ab-mq0001"


def test_next_mission_filter_null_when_no_mission_nodes(tmp_graph):
    _add("Unrelated work")

    r = runner.invoke(
        app,
        ["graph", "next", "--mission", "ab-mq0001", "--include-ideas", "--all"],
        catch_exceptions=False,
    )

    assert r.exit_code == 0, r.output
    assert json.loads(r.output) is None


def test_next_without_mission_filter_unchanged(tmp_graph):
    """No --mission: selection is unchanged (mission nodes are not excluded)."""
    node = _add("Mission work")
    _set_mission(tmp_graph, node, "ab-mq0001")

    r = runner.invoke(
        app, ["graph", "next", "--include-ideas", "--all"], catch_exceptions=False
    )

    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["id"] == node
