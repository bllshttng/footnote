"""`fno backlog ready --json` must be accepted.

`ready` already always emits JSON, but it had no --json option, so a caller
passing the flag (inbox triage.py) got a Typer exit 2 and silently fell back to
an empty backlog summary. The flag is now accepted (output unchanged).
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


def test_ready_accepts_json_flag(tmp_graph):
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-R", "title": "R", "status": "ready",
         "plan_path": "p.md", "project": "x"}
    ]}))
    r = runner.invoke(app, ["backlog", "ready", "--project", "x", "--json"],
                      catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert [e["id"] for e in json.loads(r.stdout)] == ["ab-R"]
