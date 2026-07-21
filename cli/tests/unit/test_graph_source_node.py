"""CLI-level tests for the explicit ``--source-node`` origin flag (x-d157).

The unit-level precedence chain lives in ``test_graph_provenance.py``; this file
covers the verb surface, where the fail-closed contract actually bites: an
assertion that does not resolve must refuse the command rather than file a node
that looks organically captured.
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
    g.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-aaaa",
                        "title": "The origin node",
                        "_status": "ready",
                        "domain": "code",
                        "project": "fno",
                        "slug": "the-origin-node",
                    }
                ]
            },
            indent=2,
        )
        + "\n"
    )
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    # Ambient capture must not colour these assertions: the flag is the subject.
    for var in ("FNO_NODE", "CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    return g


def _entries(g: Path) -> list[dict]:
    return json.loads(g.read_text())["entries"]


def _by_id(g: Path, node_id: str) -> dict:
    return next(e for e in _entries(g) if e["id"] == node_id)


def test_ac1_hp_explicit_source_node_stamps_the_origin(tmp_graph):
    """AC1-HP: --source-node on a filing verb stamps exactly that origin."""
    result = runner.invoke(
        app, ["backlog", "idea", "follow-up", "--source-node", "x-aaaa"]
    )
    assert result.exit_code == 0, result.output
    new_id = json.loads(result.stdout)["id"]
    assert _by_id(tmp_graph, new_id)["source_node_id"] == "x-aaaa"


def test_explicit_source_node_accepts_a_slug(tmp_graph):
    """A slug is the likely mistake and the resolver already handles it."""
    result = runner.invoke(
        app, ["backlog", "add", "follow-up", "--source-node", "the-origin-node"]
    )
    assert result.exit_code == 0, result.output
    new_id = json.loads(result.stdout)["id"]
    assert _by_id(tmp_graph, new_id)["source_node_id"] == "x-aaaa"


def test_ac1_err_unresolvable_source_node_refuses_and_writes_nothing(tmp_graph):
    """AC1-ERR: fail closed - non-zero, the token named, and no node created."""
    before = len(_entries(tmp_graph))
    result = runner.invoke(
        app, ["backlog", "idea", "follow-up", "--source-node", "x-zzzz"]
    )
    assert result.exit_code != 0
    assert "x-zzzz" in result.output
    assert len(_entries(tmp_graph)) == before


def test_ac2_err_update_rejects_a_self_reference(tmp_graph):
    """AC2-ERR: a node cannot be its own origin; the field is left untouched."""
    result = runner.invoke(
        app, ["backlog", "update", "x-aaaa", "--source-node", "x-aaaa"]
    )
    assert result.exit_code != 0
    assert "x-aaaa" in result.output
    assert _by_id(tmp_graph, "x-aaaa").get("source_node_id") is None


def test_update_sets_and_clears_the_origin(tmp_graph):
    """--source-node on update sets it; 'null' clears it, matching the flag idiom."""
    created = runner.invoke(app, ["backlog", "idea", "follow-up"])
    new_id = json.loads(created.stdout)["id"]

    assert runner.invoke(
        app, ["backlog", "update", new_id, "--source-node", "x-aaaa"]
    ).exit_code == 0
    assert _by_id(tmp_graph, new_id)["source_node_id"] == "x-aaaa"

    assert runner.invoke(
        app, ["backlog", "update", new_id, "--source-node", "null"]
    ).exit_code == 0
    assert _by_id(tmp_graph, new_id)["source_node_id"] is None


def test_ac3_edge_stale_env_origin_degrades_through_the_real_filing_path(
    tmp_graph, monkeypatch
):
    """AC3-EDGE: FNO_NODE naming a node absent from the graph stamps null.

    Exercised end-to-end rather than against _session_provenance directly,
    because the live-snapshot validation only has a snapshot to check against
    inside the locked mutator.
    """
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-stale")
    monkeypatch.setenv("FNO_NODE", "x-deleted")

    result = runner.invoke(app, ["backlog", "idea", "orphaned follow-up"])
    assert result.exit_code == 0, result.output
    node = _by_id(tmp_graph, json.loads(result.stdout)["id"])
    assert node["source_node_id"] is None
    assert "x-deleted" not in json.dumps(node)


def test_ac2_hp_env_origin_is_stamped_when_it_resolves(tmp_graph, monkeypatch):
    """AC2-HP end-to-end: a live FNO_NODE with no manifest reaches the node."""
    monkeypatch.setenv("CODEX_SESSION_ID", "sess-codex")
    monkeypatch.setenv("FNO_NODE", "x-aaaa")

    result = runner.invoke(app, ["backlog", "idea", "codex-filed follow-up"])
    assert result.exit_code == 0, result.output
    node = _by_id(tmp_graph, json.loads(result.stdout)["id"])
    assert node["source_harness"] == "codex"
    assert node["source_node_id"] == "x-aaaa"


def test_ac3_err_ambient_failure_prints_no_traceback_on_either_stream(
    tmp_graph, monkeypatch
):
    """AC3-ERR: a malformed manifest degrades quietly - exit 0, node created, no trace.

    Asserted through the filing verb rather than _session_provenance directly:
    the AC is about what an operator sees, and a traceback reaching either
    stream is the visible half of the failure.
    """
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-bad")
    (tmp_graph.parent / ".fno").mkdir(parents=True, exist_ok=True)
    (tmp_graph.parent / ".fno" / "target-state.md").write_bytes(b"\xff\xfe not utf8")
    monkeypatch.chdir(tmp_graph.parent)

    result = runner.invoke(app, ["backlog", "idea", "follow-up"])
    assert result.exit_code == 0, result.output
    assert "Traceback" not in result.output
    assert _by_id(tmp_graph, json.loads(result.stdout)["id"])["source_node_id"] is None


def test_a_stamped_origin_is_named_but_a_signalless_filing_is_quiet(tmp_graph):
    """The receipt fires on a stamped or dropped origin, never on silence.

    An always-on line would be noise on the most-used verb in the CLI, and it
    lands in the mixed output stream that callers parse as JSON.
    """
    quiet = runner.invoke(app, ["backlog", "idea", "no signal at all"])
    assert "origin:" not in quiet.output

    named = runner.invoke(
        app, ["backlog", "idea", "with an origin", "--source-node", "x-aaaa"]
    )
    assert "origin: x-aaaa" in named.output


def test_a_dropped_stale_origin_is_reported_not_swallowed(tmp_graph, monkeypatch):
    """Capture regressing to nothing must leave a trace at the moment it happens."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-stale")
    monkeypatch.setenv("FNO_NODE", "x-deleted")

    result = runner.invoke(app, ["backlog", "idea", "orphaned"])
    assert result.exit_code == 0, result.output
    assert "dropped 'x-deleted'" in result.output
