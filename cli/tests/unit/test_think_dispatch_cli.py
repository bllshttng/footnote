"""CLI tests for `fno think dispatch` (x-0a9c, Wave C).

The verb is thin glue over dispatch_conversational; these cover the glue's real
logic: live-session-id resolution, node resolution, exit codes, and that the
LIVE (session_id, cwd) pointer is what flows into the dispatch core.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.provenance import cli as think_cli
from fno.provenance.spawn_think import ThinkSpawnResult
from fno.provenance.resolver import ResolvedTranscript

runner = CliRunner()


@pytest.fixture(autouse=True)
def _clear_harness_markers(monkeypatch):
    for marker in (
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
    ):
        monkeypatch.delenv(marker, raising=False)


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
    for v in (
        "CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"
    ):
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
    posture = "codex posture: think source=codex; dispatch=claude-bg-fallback"
    assert posture in r.stderr
    assert posture not in r.stdout
    assert "think dispatched:" in r.stdout


def test_ambient_codex_json_stdout_is_single_document(graph, monkeypatch):
    monkeypatch.setenv("CODEX_THREAD_ID", "codex-thread")
    r = runner.invoke(
        think_cli.think_app,
        ["dispatch", "x-0a9c", "--json"],
    )
    assert r.exit_code == 0
    payload = json.loads(r.stdout)
    assert payload["decision"] == "spawned"
    assert payload["think_session"] == "abc123"
    assert "codex posture:" not in r.stdout
    assert "codex posture: think source=codex" in r.stderr


def test_ambient_codex_explicit_non_claude_provider_is_refused(graph, monkeypatch):
    monkeypatch.setenv("CODEX_THREAD_ID", "codex-thread")
    r = runner.invoke(
        think_cli.think_app,
        ["dispatch", "x-0a9c", "--provider", "codex"],
    )
    assert r.exit_code == 2
    posture = "codex posture: think source=codex; dispatch=unsupported"
    assert r.stderr.count(posture) == 1
    assert "detached /think uses Claude bg" in r.output
    assert "omit --provider to use the Claude fallback" in r.output
    assert "no live think-session receipt" in r.output
    assert graph == {}  # dispatch_conversational was never called


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


def test_claude_provider_flag_overlays_node(graph, monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app,
                      ["dispatch", "x-0a9c", "--provider", "claude"])
    assert r.exit_code == 0
    assert graph["node"]["provider"] == "claude"


def test_claude_source_explicit_non_claude_provider_is_refused(graph, monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "live-sid")
    r = runner.invoke(think_cli.think_app,
                      ["dispatch", "x-0a9c", "--provider", "codex"])
    assert r.exit_code == 2
    assert "detached /think uses Claude bg" in r.output
    assert "codex posture: think source=codex" not in r.output
    assert graph == {}  # dispatch_conversational was never called


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


def _subprocess_journey(tmp_path: Path, monkeypatch):
    graph_path = tmp_path / "graph.json"
    graph_path.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-0a9c",
                        "slug": "conv-think",
                        "title": "conversational verb",
                        "cwd": str(tmp_path),
                    }
                ]
            }
        )
    )
    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: graph_path)
    monkeypatch.setattr(
        "fno.provenance.spawn_think.resolve_transcript",
        lambda harness, sid, cwd, **kw: ResolvedTranscript(
            harness,
            sid,
            cwd,
            True,
            transcript_path=str(tmp_path / "transcript.jsonl"),
        ),
    )
    monkeypatch.setenv("CODEX_THREAD_ID", "codex-subprocess-thread")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    monkeypatch.setenv("FNO_HOME", str(tmp_path / "fno-home"))
    monkeypatch.setenv("HOME", str(tmp_path))

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    capture = tmp_path / "spawn-argv"
    fake = bin_dir / "fno-py"
    fake.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\0' \"$@\" > \"$SPAWN_CAPTURE\"\n"
        "printf '%s\\n' '{\"short_id\":\"real123\"}'\n"
    )
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}:{os.environ['PATH']}")
    monkeypatch.setenv("SPAWN_CAPTURE", str(capture))
    return capture


def test_codex_json_dispatch_reaches_spawn_subprocess_and_parses_receipt(
    tmp_path, monkeypatch
):
    capture = _subprocess_journey(tmp_path, monkeypatch)

    result = runner.invoke(
        think_cli.think_app,
        ["dispatch", "x-0a9c", "--json"],
    )

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["decision"] == "spawned"
    assert payload["think_session"] == "real123"
    argv = capture.read_bytes().decode().rstrip("\0").split("\0")
    assert argv[:6] == [
        "agents",
        "spawn",
        "--provider",
        "claude",
        "--substrate",
        "bg",
    ]
    assert "codex posture:" not in result.stdout
    assert "claude-bg-fallback" in result.stderr


def test_unsupported_provider_never_launches_spawn_subprocess(tmp_path, monkeypatch):
    capture = _subprocess_journey(tmp_path, monkeypatch)

    result = runner.invoke(
        think_cli.think_app,
        ["dispatch", "x-0a9c", "--provider", "codex", "--json"],
    )

    assert result.exit_code == 2
    assert not capture.exists()
    assert "dispatch=unsupported" in result.stderr
