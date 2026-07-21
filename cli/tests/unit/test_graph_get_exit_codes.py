"""US2: `fno backlog get` distinguishes an absent node from an unreadable graph.

Exit 1 KEEPS its exact meaning "graph read cleanly, node genuinely absent".
A distinct non-zero code means "could not read the graph cleanly", so the
/think router and the spawn VALIDATE step (which today treat any non-zero as
"absent") can tell a typo apart from a wedged graph.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph.cli import GRAPH_UNREADABLE_EXIT

runner = CliRunner()


@pytest.fixture
def scratch_graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    return g


def _populated(g: Path) -> None:
    g.write_text(
        json.dumps(
            {"entries": [{"id": "x-aaaa", "title": "n", "_status": "ready",
                          "project": "fno", "slug": "n"}]}
        )
        + "\n"
    )


def test_ac2hp_cmd_get_clean_miss_is_exit_1_unchanged(scratch_graph):
    _populated(scratch_graph)
    result = runner.invoke(app, ["backlog", "get", "--strict", "x-zzzz"])
    assert result.exit_code == 1, result.output
    assert f"No node matching 'x-zzzz' (id/slug/bare-hex) in {scratch_graph}" in result.output


def test_ac2hp_cmd_get_clean_miss_emits_no_new_warning(scratch_graph):
    _populated(scratch_graph)
    result = runner.invoke(app, ["backlog", "get", "--strict", "x-zzzz"])
    # A clean miss must not emit a diagnostic about reading the graph.
    assert "unreadable" not in result.output.lower()
    assert "corrupt" not in result.output.lower()


def test_ac2hp_cmd_get_resolves_present_node(scratch_graph):
    _populated(scratch_graph)
    result = runner.invoke(app, ["backlog", "get", "--strict", "x-aaaa", "--field", "id"])
    assert result.exit_code == 0, result.output
    assert result.stdout.strip() == "x-aaaa"


def test_cmd_get_unreadable_graph_is_distinct_exit_code(scratch_graph):
    scratch_graph.write_text("{ not valid json")
    result = runner.invoke(app, ["backlog", "get", "--strict", "x-d157"])
    assert result.exit_code == GRAPH_UNREADABLE_EXIT
    assert result.exit_code != 1


def test_cmd_get_unreadable_message_names_failure_not_absent(scratch_graph):
    # AC1-ERR: names the read failure + path, never asserts the node is absent.
    scratch_graph.write_text("{ not valid json")
    result = runner.invoke(app, ["backlog", "get", "--strict", "x-d157"])
    assert "No node matching" not in result.output
    assert str(scratch_graph) in result.output


def test_cmd_get_malformed_root_is_distinct_exit_code(scratch_graph):
    # A root object with no 'entries' key is unreadable, not an absent node.
    scratch_graph.write_text(json.dumps({}))
    result = runner.invoke(app, ["backlog", "get", "--strict", "x-d157"])
    assert result.exit_code == GRAPH_UNREADABLE_EXIT
    assert "No node matching" not in result.output


def test_cmd_get_empty_graph_is_a_clean_miss(scratch_graph):
    # {"entries": []} is a legitimately empty graph: an absent node, exit 1.
    scratch_graph.write_text(json.dumps({"entries": []}))
    result = runner.invoke(app, ["backlog", "get", "--strict", "x-d157"])
    assert result.exit_code == 1
    assert "No node matching" in result.output
