"""Backlog selection must never surface a completed node.

read_graph does not recompute _status, so a node closed out of band (PR merged
via reconcile/done in another process) can carry completed_at while its
persisted _status is still a stale "ready". Without a completed_at guard,
`fno backlog next` returns it and `advance` dispatches a /target worker for an
already-done node.
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
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def test_completed_node_excluded_from_next_and_ready(tmp_graph):
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-DONE", "title": "done", "_status": "ready",
         "plan_path": "p.md", "project": "x",
         "completed_at": "2026-06-20T00:00:00Z"},
        {"id": "ab-LIVE", "title": "live", "_status": "ready",
         "plan_path": "q.md", "project": "x"},
    ]}))

    nxt = runner.invoke(app, ["backlog", "next", "--project", "x"],
                        catch_exceptions=False)
    assert nxt.exit_code == 0, nxt.output
    assert json.loads(nxt.stdout)["id"] == "ab-LIVE"

    rdy = runner.invoke(app, ["backlog", "ready", "--project", "x"],
                        catch_exceptions=False)
    assert rdy.exit_code == 0, rdy.output
    assert [e["id"] for e in json.loads(rdy.stdout)] == ["ab-LIVE"]
