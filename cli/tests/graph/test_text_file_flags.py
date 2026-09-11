"""Rank 3 flags on the graph verbs: `note --body-file`, `idea --details-file`.

The acceptance is a round-trip: a body with quotes and newlines - the shape
that a worktree session's Bash cannot carry positionally - arrives byte-exact
from the file and is stored unharmed.

Filter: `fno doctor test cli/tests/graph/test_text_file_flags.py`
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.graph import cli as graph_cli
from fno.graph.cli import cli
from fno.graph.store import locked_mutate_graph, read_graph

runner = CliRunner()

BODY = 'note with "quotes" and\na newline\n'


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": []}), encoding="utf-8")
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: g)
    return g


def _seed_node(g, node_id="x-eeee"):
    def _add(entries):
        entries.append(
            {
                "id": node_id,
                "title": f"node {node_id}",
                "project": "fno",
                "type": "feature",
                "priority": "p2",
                "status": "ready",
                "blocked_by": [],
                "children": [],
            }
        )
        return entries

    locked_mutate_graph(g, _add)


def test_note_body_file_roundtrip_quotes_and_newlines(tmp_graph):
    _seed_node(tmp_graph)
    body_file = tmp_graph.parent / "note.md"
    body_file.write_text(BODY, encoding="utf-8")
    r = runner.invoke(
        cli, ["note", "x-eeee", "--body-file", str(body_file), "--quiet", "-J"]
    )
    assert r.exit_code == 0, r.output
    stored = json.loads(r.stdout.strip().splitlines()[-1])
    assert stored["note"]["text"] == BODY.strip()


def test_note_body_file_and_positional_refused(tmp_graph):
    _seed_node(tmp_graph)
    body_file = tmp_graph.parent / "note.md"
    body_file.write_text(BODY, encoding="utf-8")
    r = runner.invoke(cli, ["note", "x-eeee", "positional", "--body-file", str(body_file)])
    assert r.exit_code == 1
    assert "not both" in r.stderr


def test_idea_details_file_roundtrip(tmp_graph):
    details = 'guidance with "quotes" and\nnewlines\n'
    details_file = tmp_graph.parent / "details.md"
    details_file.write_text(details, encoding="utf-8")
    r = runner.invoke(
        cli,
        ["idea", "file-fed idea", "--details-file", str(details_file),
         "--difficulty", "low", "-J"],
    )
    assert r.exit_code == 0, r.output
    receipt = json.loads(r.stdout)
    minted = receipt["id"]
    assert minted, "expected a minted node"
    node = next(e for e in read_graph(tmp_graph) if e.get("id") == minted)
    assert node["details"] == details


def test_idea_details_file_and_details_refused(tmp_graph):
    details_file = tmp_graph.parent / "details.md"
    details_file.write_text("d", encoding="utf-8")
    r = runner.invoke(
        cli,
        ["idea", "t", "--details-file", str(details_file), "--details", "inline"],
    )
    assert r.exit_code == 1
    assert "not both" in r.stderr
