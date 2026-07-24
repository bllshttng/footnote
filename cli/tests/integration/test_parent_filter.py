"""Integration tests for the --parent epic-scope filter (C2, ab-facfaade).

`fno backlog next --parent <epic>` / `ready --parent <epic>` restrict
candidates to the transitive children of an epic so a walk can drain one
epic's subtree. Mirrors the existing --roadmap-id filter.
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
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    return g


def _add(title, **opts) -> str:
    args = ["graph", "add", title]
    for k, v in opts.items():
        args += [f"--{k}", str(v)]
    r = runner.invoke(app, args, catch_exceptions=False)
    assert r.exit_code == 0, r.output
    # raw_decode tolerates trailing stderr (the filing-time dedup receipt
    # CliRunner mixes into r.output when the new node resembles an existing one).
    return json.JSONDecoder().raw_decode(r.output)[0]["id"]


def _set_parent(child_id, parent_id):
    r = runner.invoke(
        app, ["graph", "update", child_id, "--parent", parent_id],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output


def _epic_with_children(tmp_graph):
    epic = _add("Epic")
    c1 = _add("Child one")
    c2 = _add("Child two")
    loose = _add("Loose node")
    _set_parent(c1, epic)
    _set_parent(c2, epic)
    return epic, c1, c2, loose


def test_ac2_hp_next_parent_scopes_to_children(tmp_graph):
    """`next --parent <epic>` only ever returns a child of the epic."""
    epic, c1, c2, loose = _epic_with_children(tmp_graph)
    r = runner.invoke(
        app, ["graph", "next", "--parent", epic, "--include-ideas", "--all"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    picked = json.loads(r.output)
    assert picked is not None
    assert picked["id"] in {c1, c2}
    assert picked["id"] != loose


def test_ac2_hp_ready_parent_scopes_to_children(tmp_graph):
    """`ready --parent <epic>` lists only the epic's children, not loose nodes."""
    epic, c1, c2, loose = _epic_with_children(tmp_graph)
    r = runner.invoke(
        app, ["graph", "ready", "--parent", epic, "--include-ideas", "--all"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    ids = {e["id"] for e in json.loads(r.output)}
    assert ids == {c1, c2}
    assert loose not in ids
    assert epic not in ids


def test_ac2_err_next_missing_parent_exits_nonzero(tmp_graph):
    """`--parent ab-doesnotexist` is a hard error, not silent nothing."""
    _epic_with_children(tmp_graph)
    r = runner.invoke(
        app, ["graph", "next", "--parent", "ab-doesnotexist", "--all"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0
    assert "no such node" in r.output.lower() or "not found" in r.output.lower()


def test_ac2_edge_parent_with_no_children_emits_message(tmp_graph):
    """A valid node with no children returns null + a 'no children' note,
    so the walker can fall back rather than treating it as an error."""
    epic, c1, c2, loose = _epic_with_children(tmp_graph)
    r = runner.invoke(
        app, ["graph", "next", "--parent", loose, "--include-ideas", "--all"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    # null payload on stdout, advisory message somewhere in output.
    assert "null" in r.output
    assert "no children under" in r.output.lower()


def test_parent_combines_with_priority_order(tmp_graph):
    """Within an epic, higher-priority children come first."""
    epic = _add("Epic")
    lo = _add("low child", priority="p3")
    hi = _add("high child", priority="p1")
    _set_parent(lo, epic)
    _set_parent(hi, epic)
    r = runner.invoke(
        app, ["graph", "next", "--parent", epic, "--include-ideas", "--all"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["id"] == hi
