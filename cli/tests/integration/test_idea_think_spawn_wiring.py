"""Integration: `fno backlog idea` wires the born-with-why birth hook (x-6a10).

These prove the WIRING contract in cmd_idea (the hook's own behavior is covered
exhaustively in tests/unit/test_spawn_think.py):

- the freshly-persisted node (with its x-30f6 provenance stamp) is handed to
  maybe_spawn_think exactly once;
- a raising hook is non-fatal: the node is still filed and the verb exits 0
  (Failure Modes: node birth never fails because of the spawn leg).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.graph.cli as gc
import fno.graph.store as gs
import fno.provenance.spawn_think as st


@pytest.fixture
def graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Isolate the graph + render + lock under tmp_path."""
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": []}) + "\n")
    monkeypatch.setattr(gc, "_graph_path", lambda: g)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    # An origin so the persisted node is eligible (source_session_id populated).
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "testsid-1234")
    monkeypatch.chdir(tmp_path)

    def entries():
        return json.loads(g.read_text())["entries"]

    return g, entries


def test_idea_invokes_birth_hook_with_persisted_node(graph, monkeypatch):
    """The persisted, provenance-stamped node reaches maybe_spawn_think once."""
    _, entries = graph
    seen: list[dict] = []
    monkeypatch.setattr(st, "maybe_spawn_think", lambda node, **k: seen.append(node))

    res = CliRunner().invoke(
        __import__("fno.cli", fromlist=["app"]).app,
        ["backlog", "idea", "A real generated idea"],
    )

    assert res.exit_code == 0, res.output
    created_id = json.loads(res.output)["id"]
    assert len(seen) == 1
    assert seen[0]["id"] == created_id
    # The node carries the x-30f6 ambient provenance stamp (origin present).
    assert seen[0]["source_session_id"] == "testsid-1234"
    # And it is the actually-persisted node, not a pre-persist copy.
    assert any(e["id"] == created_id for e in entries())


def test_idea_hook_failure_is_non_fatal(graph, monkeypatch):
    """A raising hook never blocks the filing of the node (exit 0, node persisted)."""
    _, entries = graph

    def boom(node, **k):
        raise RuntimeError("spawn leg exploded")

    monkeypatch.setattr(st, "maybe_spawn_think", boom)

    res = CliRunner().invoke(
        __import__("fno.cli", fromlist=["app"]).app,
        ["backlog", "idea", "Idea that survives a hook crash"],
    )

    assert res.exit_code == 0, res.output
    created_id = json.loads(res.output)["id"]
    assert any(e["id"] == created_id for e in entries())
