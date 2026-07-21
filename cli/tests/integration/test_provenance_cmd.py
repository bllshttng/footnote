"""Integration tests for `fno backlog provenance <node-id>` (Task 2.3).

Uses the same tmp_graph fixture pattern as test_graph_cli.py.
The resolver's projects_root is injected via monkeypatch so no real
~/.claude is touched.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """A fresh empty graph.json; monkeypatches fno.graph constants to use it."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    return g


def _invoke(*args, input=None):
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _write_node(g: Path, node: dict) -> None:
    data = json.loads(g.read_text())
    data["entries"].append(node)
    g.write_text(json.dumps(data, indent=2) + "\n")


def _base_node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "title": "Test node",
        "status": "ready",
        "domain": "code",
        "project": "fno",
        "cwd": "/Users/bb16/code/footnote",
        "source_session_id": None,
        "source_harness": None,
        "source_node_id": None,
        "source_plan_path": None,
        "spawned_by_session": None,
        "spawned_by_harness": None,
        "spawned_by_cwd": None,
    }
    base.update(overrides)
    return base


# ---------------------------------------------------------------------------
# AC-HP: node with no provenance pointers - human output
# ---------------------------------------------------------------------------

def test_ac_hp_no_provenance_exits_zero(tmp_graph):
    """AC-HP: node with all provenance fields null -> exits 0, prints summary."""
    _write_node(tmp_graph, _base_node("ab-prov0001"))

    r = _invoke("backlog", "provenance", "ab-prov0001")
    assert r.exit_code == 0, r.output
    # Should print the node id in the output
    assert "ab-prov0001" in r.output


# ---------------------------------------------------------------------------
# AC-HP: node with source_session_id (node-birth edge) - file resolved
# ---------------------------------------------------------------------------

def test_ac_hp_source_edge_resolved(tmp_graph, tmp_path, monkeypatch):
    """AC-HP: node with source_session_id + matching transcript -> resolved path shown."""
    session_id = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"
    cwd = "/Users/bb16/code/me/abilities"

    # Build a fake projects_root with the transcript file
    slug = cwd.replace("/", "-").replace(".", "-")
    proj_dir = tmp_path / "projects" / slug
    proj_dir.mkdir(parents=True, exist_ok=True)
    transcript = proj_dir / f"{session_id}.jsonl"
    transcript.write_text('{"type":"summary"}\n')
    projects_root = tmp_path / "projects"

    # Inject the projects_root into the resolver
    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", projects_root)

    node = _base_node(
        "ab-prov0002",
        source_session_id=session_id,
        source_harness="claude",
        cwd=cwd,
    )
    _write_node(tmp_graph, node)

    r = _invoke("backlog", "provenance", "ab-prov0002")
    assert r.exit_code == 0, r.output
    assert session_id in r.output
    assert str(transcript) in r.output


def test_node_birth_resolves_via_source_cwd_not_durable_cwd(tmp_graph, tmp_path, monkeypatch):
    """gemini review: a node filed from a worktree resolves via source_cwd.

    The node's durable `cwd` is the canonical project root; the transcript dir
    is slugged by the SESSION cwd (the worktree). Resolution must use source_cwd
    so the worktree case (the common mid-pipeline case) does not report not-found.
    """
    session_id = "abcd1234-0000-0000-0000-000000000000"
    session_cwd = "/Users/bb16/conductor/workspaces/footnote/wt-feature"  # worktree
    durable_cwd = "/Users/bb16/code/footnote"                              # canonical root

    slug = session_cwd.replace("/", "-").replace(".", "-")
    proj_dir = tmp_path / "projects" / slug
    proj_dir.mkdir(parents=True, exist_ok=True)
    transcript = proj_dir / f"{session_id}.jsonl"
    transcript.write_text('{"type":"summary"}\n')

    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", tmp_path / "projects")

    node = _base_node(
        "ab-prov00wt",
        source_session_id=session_id,
        source_harness="claude",
        source_cwd=session_cwd,   # session/worktree cwd
        cwd=durable_cwd,          # durable project root (would resolve to nothing)
    )
    _write_node(tmp_graph, node)

    r = _invoke("backlog", "provenance", "ab-prov00wt")
    assert r.exit_code == 0, r.output
    assert str(transcript) in r.output  # resolved via source_cwd, not durable cwd


# ---------------------------------------------------------------------------
# AC-HP: --json flag returns structured object
# ---------------------------------------------------------------------------

def test_ac_hp_json_flag_returns_structured(tmp_graph, tmp_path, monkeypatch):
    """AC-HP: --json flag -> machine-readable object with 'node_id' + edges."""
    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", tmp_path / "empty")

    node = _base_node(
        "ab-prov0003",
        source_session_id="sid-abc",
        source_harness="claude",
        cwd="/tmp/myproject",
    )
    _write_node(tmp_graph, node)

    r = _invoke("backlog", "provenance", "ab-prov0003", "--json")
    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["node_id"] == "ab-prov0003"
    assert "node_birth" in data or "source_edge" in data or "edges" in data


# ---------------------------------------------------------------------------
# AC-HP: spawned_by edge resolution uses spawned_by_cwd
# ---------------------------------------------------------------------------

def test_ac_hp_spawn_edge_uses_spawned_by_cwd(tmp_graph, tmp_path, monkeypatch):
    """AC-HP: spawned_by_session uses spawned_by_cwd for slug resolution."""
    session_id = "deadbeef-cafe-babe-0000-000000000001"
    spawn_cwd = "/Users/bb16/code/regready"

    slug = spawn_cwd.replace("/", "-").replace(".", "-")
    proj_dir = tmp_path / "projects" / slug
    proj_dir.mkdir(parents=True, exist_ok=True)
    transcript = proj_dir / f"{session_id}.jsonl"
    transcript.write_text("{}\n")
    projects_root = tmp_path / "projects"

    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", projects_root)

    node = _base_node(
        "ab-prov0004",
        spawned_by_session=session_id,
        spawned_by_harness="claude",
        spawned_by_cwd=spawn_cwd,
    )
    _write_node(tmp_graph, node)

    r = _invoke("backlog", "provenance", "ab-prov0004")
    assert r.exit_code == 0, r.output
    assert session_id in r.output
    assert str(transcript) in r.output


# ---------------------------------------------------------------------------
# AC-EDGE: node not found -> non-zero exit
# ---------------------------------------------------------------------------

def test_ac_edge_node_not_found(tmp_graph):
    """AC-EDGE: unknown node id -> non-zero exit, no crash."""
    r = _invoke("backlog", "provenance", "ab-nosuchnode")
    assert r.exit_code != 0


# ---------------------------------------------------------------------------
# AC-EDGE: foreign harness in spawn edge -> resolved=False, no crash
# ---------------------------------------------------------------------------

def test_ac_edge_foreign_harness_spawn(tmp_graph, tmp_path, monkeypatch):
    """AC-EDGE: spawned_by_harness=codex -> resolved=False, no exception."""
    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", tmp_path / "empty")

    node = _base_node(
        "ab-prov0005",
        spawned_by_session="codex-session-xyz",
        spawned_by_harness="codex",
        spawned_by_cwd="/some/path",
    )
    _write_node(tmp_graph, node)

    r = _invoke("backlog", "provenance", "ab-prov0005")
    assert r.exit_code == 0, r.output
    # Should mention the session id even though unresolved
    assert "codex-session-xyz" in r.output


# ---------------------------------------------------------------------------
# Read-only: no mutation to graph.json
# ---------------------------------------------------------------------------

def test_ac_verify_read_only(tmp_graph, tmp_path, monkeypatch):
    """AC-VERIFY: provenance command never mutates graph.json."""
    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", tmp_path / "empty")

    node = _base_node("ab-prov0006", source_session_id="sid-xyz", source_harness="claude")
    _write_node(tmp_graph, node)

    before = tmp_graph.read_text()
    _invoke("backlog", "provenance", "ab-prov0006")
    after = tmp_graph.read_text()

    assert before == after


# ---------------------------------------------------------------------------
# x-b6e4 - lifecycle `sessions` rows in provenance output
# ---------------------------------------------------------------------------

_SESSIONS = [
    {"phase": "think", "harness": "claude", "session_id": "S1", "at": "2026-07-12T01:00:00Z"},
    {"phase": "blueprint", "harness": "claude", "session_id": "S1", "at": "2026-07-12T02:00:00Z"},
    {"phase": "ship", "harness": "codex", "session_id": "S2", "at": "2026-07-12T03:00:00Z"},
]


def test_ac3_ui_lifecycle_rows_human(tmp_graph, tmp_path, monkeypatch):
    """AC3-UI: a node with lifecycle entries shows an append-ordered phase table,
    while the existing birth edge stays visible."""
    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", tmp_path / "empty")

    _write_node(tmp_graph, _base_node(
        "ab-life0001", source_session_id="birth-sid", source_harness="claude",
        sessions=list(_SESSIONS),
    ))

    r = _invoke("backlog", "provenance", "ab-life0001")
    assert r.exit_code == 0, r.output
    # all four phase rows present, in append order
    for tok in ("think", "blueprint", "ship", "S1", "S2", "2026-07-12T03:00:00Z"):
        assert tok in r.output
    assert r.output.index("think") < r.output.index("blueprint") < r.output.index("ship")
    # birth edge still rendered
    assert "birth-sid" in r.output


def test_ac3_ui_lifecycle_rows_json(tmp_graph, tmp_path, monkeypatch):
    """AC3-UI (JSON): --json carries the sessions list in append order alongside edges."""
    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", tmp_path / "empty")

    _write_node(tmp_graph, _base_node("ab-life0002", sessions=list(_SESSIONS)))

    r = _invoke("backlog", "provenance", "ab-life0002", "--json")
    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["sessions"] == _SESSIONS
    assert "edges" in data  # existing edges preserved


def test_lifecycle_empty_sessions_no_crash(tmp_graph, tmp_path, monkeypatch):
    """A legacy node with no lifecycle entries renders fine (empty list)."""
    import fno.provenance.resolver as resolver_mod
    monkeypatch.setattr(resolver_mod, "_DEFAULT_PROJECTS_ROOT", tmp_path / "empty")

    _write_node(tmp_graph, _base_node("ab-life0003"))

    r = _invoke("backlog", "provenance", "ab-life0003", "--json")
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["sessions"] == []
