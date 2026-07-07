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
    for v in ("CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(v, raising=False)
    r = runner.invoke(think_cli.think_app, ["dispatch", "x-0a9c"])
    assert r.exit_code == 2
    assert "no live session id" in r.output
    assert "node" not in graph  # never reached the dispatch


def test_ambient_codex_session(graph, monkeypatch):
    """codex P2: a codex session (no CLAUDE_CODE_SESSION_ID) still dispatches -
    the live pointer is detected ambiently across all three harnesses."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    monkeypatch.delenv("GEMINI_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_SESSION_ID", "codex-sid")
    r = runner.invoke(think_cli.think_app, ["dispatch", "x-0a9c"])
    assert r.exit_code == 0
    assert graph["session_id"] == "codex-sid"
    assert graph["harness"] == "codex"


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


def test_model_flag_overlays_node(graph, monkeypatch):
    """AC1-HP/AC1-UI: --model rides onto the node so it reaches the spawn seam."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app,
                      ["dispatch", "x-0a9c", "--model", "glm-4.7"])
    assert r.exit_code == 0
    assert graph["node"]["model"] == "glm-4.7"


def test_provider_flag_overlays_node(graph, monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app,
                      ["dispatch", "x-0a9c", "--provider", "codex"])
    assert r.exit_code == 0
    assert graph["node"]["provider"] == "codex"


def test_empty_model_rejected_exits_2(graph, monkeypatch):
    """AC2-ERR at this verb: an empty --model is a usage error, nothing dispatches."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app,
                      ["dispatch", "x-0a9c", "--model", "   "])
    assert r.exit_code == 2
    assert "--model must not be empty" in r.output
    assert "node" not in graph  # never reached the dispatch


def test_no_pins_leaves_node_unpinned(graph, monkeypatch):
    """Byte-for-byte: without flags the node carries no model/provider key."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app, ["dispatch", "x-0a9c"])
    assert r.exit_code == 0
    assert "model" not in graph["node"]
    assert "provider" not in graph["node"]


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
