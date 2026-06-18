"""Unit tests for graph provenance schema extension (Phase 01).

Tests for:
- Task 1.1: _apply_graph_defaults adds provenance fields via setdefault
- Task 1.3: query_by_source_inbox_msg helper in load.py
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_graph(tmp_path: Path, entries: list[dict]) -> Path:
    """Write a graph.json with the given entries and return its path."""
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")
    return g


def _patch_graph(monkeypatch, graph_path: Path) -> None:
    """Redirect the graph module constants to a tmp graph."""
    import fno.graph._constants as gc
    import fno.graph.store as gs
    lock = graph_path.parent / "graph.lock"
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", graph_path.parent / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", lock)
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", lock)


# ---------------------------------------------------------------------------
# Task 1.1 - provenance defaults via _apply_graph_defaults
# ---------------------------------------------------------------------------


def test_ac1_hp_existing_entry_gains_provenance_defaults():
    """AC1-HP: Entry without provenance fields gains defaults on _apply_graph_defaults."""
    from fno.graph.store import _apply_graph_defaults

    entries = [
        {
            "id": "ab-legacy01",
            "title": "Old entry",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
        }
    ]
    result = _apply_graph_defaults(entries)
    e = result[0]
    assert e["source_kind"] == "organic"
    assert e["source_project"] is None
    assert e["source_session_id"] is None
    assert e["source_inbox_msg"] is None
    # Existing field unchanged
    assert e["title"] == "Old entry"
    assert e["domain"] == "code"


def test_ac2_err_existing_source_kind_preserved():
    """AC2-ERR: setdefault is a no-op when source_kind is already set."""
    from fno.graph.store import _apply_graph_defaults

    entries = [
        {
            "id": "ab-inbox001",
            "title": "From inbox",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_kind": "from_inbox",
            "source_inbox_msg": "msg-a4f1",
        }
    ]
    result = _apply_graph_defaults(entries)
    e = result[0]
    assert e["source_kind"] == "from_inbox"
    assert e["source_inbox_msg"] == "msg-a4f1"


def test_ac4_edge_kanban_render_does_not_crash_with_provenance_fields(tmp_path, monkeypatch):
    """AC4-EDGE: kanban renderer ignores provenance fields, no crash on entries with them."""
    g = _make_graph(tmp_path, [
        {
            "id": "ab-kan00001",
            "title": "Kanban entry",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_kind": "from_inbox",
            "source_project": "example-pipeline",
            "source_session_id": "sess-abc",
            "source_inbox_msg": "msg-a4f1",
        }
    ])
    _patch_graph(monkeypatch, g)

    from fno.graph.render import render_graph_md
    from fno.graph.store import read_graph

    entries = read_graph(g)
    out_path = tmp_path / "graph.md"
    # Must not crash
    render_graph_md(entries, path=out_path)
    assert out_path.exists()
    md = out_path.read_text()
    # Provenance fields must NOT appear in kanban output
    assert "source_inbox_msg" not in md
    assert "msg-a4f1" not in md
    assert "source_kind" not in md


def test_ac4_edge_provenance_survives_save_reload(tmp_path, monkeypatch):
    """AC4-EDGE: Idempotent triage - source_inbox_msg persists across save/reload."""
    g = _make_graph(tmp_path, [
        {
            "id": "ab-triage01",
            "title": "Triage entry",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_inbox_msg": "msg-a4f1",
            "source_kind": "from_inbox",
        }
    ])
    _patch_graph(monkeypatch, g)

    from fno.graph.store import read_graph, locked_mutate_graph

    # Verify field is queryable after read
    entries = read_graph(g)
    assert entries[0]["source_inbox_msg"] == "msg-a4f1"

    # Trigger a save/reload cycle via locked_mutate_graph (identity mutation)
    def identity(es: list[dict]) -> list[dict]:
        return es

    locked_mutate_graph(g, identity)

    # Reload and verify persistence
    reloaded = read_graph(g)
    assert len(reloaded) == 1
    assert reloaded[0]["source_inbox_msg"] == "msg-a4f1"
    assert reloaded[0]["source_kind"] == "from_inbox"


# ---------------------------------------------------------------------------
# Task 1.3 - query_by_source_inbox_msg helper
# ---------------------------------------------------------------------------


def test_ac1_hp_query_by_source_inbox_msg_single_match(tmp_path, monkeypatch):
    """AC1-HP: Single match returns list of one entry."""
    g = _make_graph(tmp_path, [
        {
            "id": "ab-qry00001",
            "title": "Entry with msg",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_inbox_msg": "msg-a4f1",
        },
        {
            "id": "ab-qry00002",
            "title": "Entry without msg",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
        },
    ])
    _patch_graph(monkeypatch, g)

    from fno.graph.load import query_by_source_inbox_msg
    results = query_by_source_inbox_msg("msg-a4f1", path=g)
    assert len(results) == 1
    assert results[0]["id"] == "ab-qry00001"


def test_ac2_err_query_by_source_inbox_msg_no_match(tmp_path, monkeypatch):
    """AC2-ERR: No match returns empty list without error."""
    g = _make_graph(tmp_path, [
        {
            "id": "ab-qry00003",
            "title": "No msg here",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
        },
    ])
    _patch_graph(monkeypatch, g)

    from fno.graph.load import query_by_source_inbox_msg
    results = query_by_source_inbox_msg("msg-nonexistent", path=g)
    assert results == []


def test_ac4_edge_query_returns_both_on_duplicate(tmp_path, monkeypatch):
    """AC4-EDGE: Two entries with same source_inbox_msg both returned (duplicate detection)."""
    g = _make_graph(tmp_path, [
        {
            "id": "ab-dup00001",
            "title": "Dupe entry A",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_inbox_msg": "msg-dupe",
        },
        {
            "id": "ab-dup00002",
            "title": "Dupe entry B",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_inbox_msg": "msg-dupe",
        },
    ])
    _patch_graph(monkeypatch, g)

    from fno.graph.load import query_by_source_inbox_msg
    results = query_by_source_inbox_msg("msg-dupe", path=g)
    assert len(results) == 2
    ids = {e["id"] for e in results}
    assert ids == {"ab-dup00001", "ab-dup00002"}
