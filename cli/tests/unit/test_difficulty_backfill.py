"""Acceptance tests for the reversible difficulty coverage backfill."""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    graph = tmp_path / "graph.json"
    graph.write_text('{"entries": []}\n')
    import fno.graph._constants as constants
    import fno.graph.store as store

    monkeypatch.setattr(constants, "GRAPH_JSON", graph)
    monkeypatch.setattr(constants, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(constants, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(constants, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(store, "GRAPH_JSON", graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    return graph

def test_difficulty_backfill_writes_attributed_sample():
    from fno.graph.migrations import backfill_difficulty

    entries = [
        {"id": "x-large", "size": "L", "difficulty": None},
        {
            "id": "x-existing",
            "size": "S",
            "difficulty": "high",
            "difficulty_history": [{"value": "high", "source": "filed"}],
        },
        {"id": "x-unknown", "title": "no signal", "difficulty": None},
    ]

    receipt = backfill_difficulty(entries, apply=True)

    assert entries[0]["difficulty"] == "high"
    assert entries[0]["difficulty_history"][-1]["source"] == "backfill"
    assert entries[1]["difficulty"] == "high"
    assert entries[1]["difficulty_history"] == [{"value": "high", "source": "filed"}]
    assert entries[2]["difficulty"] is None
    assert "x-unknown" in receipt["skipped"]


def test_migrate_difficulty_backfill_cli_writes_positive_sample(tmp_graph):
    tmp_graph.write_text(
        json.dumps({"entries": [{"id": "x-cli-large", "size": "L", "difficulty": None}]})
        + "\n"
    )

    result = runner.invoke(
        app, ["backlog", "migrate-difficulty", "--backfill", "--apply"]
    )

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output)
    assert receipt["written"] == ["x-cli-large"]
    row = json.loads(tmp_graph.read_text())["entries"][0]
    assert row["difficulty"] == "high"
    assert row["difficulty_history"][-1]["source"] == "backfill"
