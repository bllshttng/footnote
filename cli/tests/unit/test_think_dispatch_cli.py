"""CLI tests for `fno think dispatch` (x-0a9c, Wave C).

The verb is thin glue over dispatch_conversational; these cover the glue's real
logic: live-session-id resolution, node resolution, exit codes, and that the
LIVE (session_id, cwd) pointer is what flows into the dispatch core.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.provenance import cli as think_cli
from fno.provenance.spawn_think import ThinkSpawnResult

runner = CliRunner()


@pytest.fixture
def graph(monkeypatch, tmp_path):
    """A one-node graph + a captured dispatch seam. Returns the capture dict."""
    entries = [{"id": "x-0a9c", "slug": "conv-think", "title": "conversational verb",
                "source_session_id": "stored", "source_cwd": "/birth"}]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda p: entries)
    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: tmp_path / "graph.json")
    cap: dict = {}

    def fake_dispatch(node, *, session_id, cwd, harness="claude", **kw):
        cap.update(node=node, session_id=session_id, cwd=cwd, harness=harness)
        return ThinkSpawnResult("spawned", "think_spawned", node_id=node["id"],
                                presence="attended", resolved=True, think_session="abc123")

    monkeypatch.setattr("fno.provenance.spawn_think.dispatch_conversational", fake_dispatch)
    return cap


def test_no_live_session_exits_2(graph, monkeypatch):
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    r = runner.invoke(think_cli.think_app, ["dispatch", "x-0a9c"])
    assert r.exit_code == 2
    assert "no live session id" in r.output
    assert "node" not in graph  # never reached the dispatch


def test_node_not_found_exits_2(graph, monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app, ["dispatch", "no-such-node-xyz"])
    assert r.exit_code == 2
    assert "no node matches" in r.output


def test_happy_path_passes_live_pointer(graph, monkeypatch):
    """AC5-HP at the CLI boundary: the LIVE session id + cwd reach the core."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app,
                      ["dispatch", "x-0a9c", "--cwd", "/live/here"])
    assert r.exit_code == 0
    assert "think dispatched: x-0a9c" in r.output
    assert graph["session_id"] == "live-sid"
    assert graph["cwd"] == "/live/here"
    assert graph["node"]["id"] == "x-0a9c"


def test_skipped_exits_1(graph, monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    monkeypatch.setattr(
        "fno.provenance.spawn_think.dispatch_conversational",
        lambda node, **kw: ThinkSpawnResult(
            "skipped", "think_skipped", reason="already-claimed", node_id=node["id"]),
    )
    r = runner.invoke(think_cli.think_app, ["dispatch", "x-0a9c"])
    assert r.exit_code == 1
    assert "already-claimed" in r.output
