"""Receipts must be backed by a store read-back."""

from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


def _graph(tmp_path: Path, monkeypatch, entries: list[dict]) -> Path:
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": entries}) + "\n")
    import fno.graph._constants as constants
    import fno.graph.store as store

    monkeypatch.setattr(constants, "GRAPH_JSON", graph)
    monkeypatch.setattr(constants, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(store, "GRAPH_JSON", graph)
    return graph


def test_idea_refuses_when_the_new_row_does_not_read_back(tmp_path, monkeypatch):
    _graph(tmp_path, monkeypatch, [])
    monkeypatch.setattr("fno.graph.store._readback_row", lambda path, node_id: (None, True))

    result = runner.invoke(
        app,
        ["backlog", "idea", "lost filing", "--difficulty", "low", "--separate"],
    )

    assert result.exit_code == 1, result.output
    assert "write did not land" in result.output


def test_update_refuses_when_the_row_does_not_read_back(tmp_path, monkeypatch):
    graph = _graph(
        tmp_path,
        monkeypatch,
        [{"id": "ab-00000001", "title": "old", "domain": "code", "project": "p"}],
    )
    monkeypatch.setattr("fno.graph.load.load_graph", lambda path=None: [])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--title", "new"])

    assert result.exit_code == 1, result.output
    assert "write did not land" in result.output
    from fno.graph.store import read_graph_strict

    assert read_graph_strict(graph)[0]["title"] == "new"


def test_update_receipt_names_the_resolved_id(tmp_path, monkeypatch):
    _graph(
        tmp_path,
        monkeypatch,
        [{"id": "ab-00000001", "title": "old", "domain": "code", "project": "p"}],
    )

    result = runner.invoke(app, ["backlog", "update", "ab-0000", "--title", "new"])

    assert result.exit_code == 0, result.output
    assert "Updated ab-00000001" in result.output
    assert "Updated ab-0000" not in result.output.splitlines()


def test_update_readback_uses_resolved_id_when_prefix_becomes_ambiguous(tmp_path, monkeypatch):
    _graph(
        tmp_path,
        monkeypatch,
        [{"id": "ab-00000001", "title": "old", "domain": "code", "project": "p"}],
    )
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda path=None: [
            {"id": "ab-00000001", "title": "new"},
            {"id": "ab-00000002", "title": "other"},
        ],
    )

    result = runner.invoke(app, ["backlog", "update", "ab-0000", "--title", "new"])

    assert result.exit_code == 0, result.output
    assert "Updated ab-00000001" in result.output
