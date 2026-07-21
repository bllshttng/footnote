"""`backlog update --pr-number/--pr-url null` clears the link.

Every other nullable scalar on this verb documents `'null' clears`; these two
did not honor it, and the graph is hand-edit-forbidden - so a node linked to the
wrong PR could not be unlinked at all, and the mislink would ride to a merge and
close a node that shipped nothing.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _first(g: Path) -> dict:
    return json.loads(g.read_text())["entries"][0]


def test_pr_link_can_be_cleared(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_number": 504, "pr_url": "https://example.com/pull/504",
    }])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-number", "null", "--pr-url", "null",
    ])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] is None
    assert node["pr_url"] is None


def test_pr_number_still_sets_an_int(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "77", "--pr-url", "https://example.com/pull/77",
    ])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] == 77
    assert node["pr_url"] == "https://example.com/pull/77"
