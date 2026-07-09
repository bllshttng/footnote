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
# x-30f6 - parent-edge provenance fields (source_node_id + spawned_by_*)
# ---------------------------------------------------------------------------


def test_ac_hp_new_provenance_fields_default_null():
    """AC (x-30f6): a legacy entry gains source_node_id + spawned_by_* as None."""
    from fno.graph.store import _apply_graph_defaults

    entries = [
        {
            "id": "ab-legacy02",
            "title": "Old entry",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
        }
    ]
    e = _apply_graph_defaults(entries)[0]
    assert e["source_node_id"] is None
    assert e["spawned_by_session"] is None
    assert e["spawned_by_harness"] is None
    assert e["spawned_by_cwd"] is None


def test_ac_err_existing_parent_edge_preserved():
    """AC (x-30f6): setdefault is a no-op when the parent-edge fields are set."""
    from fno.graph.store import _apply_graph_defaults

    entries = [
        {
            "id": "ab-edge0001",
            "title": "Spawned entry",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_node_id": "ab-origin01",
            "spawned_by_session": "4ec8a08b",
            "spawned_by_harness": "claude",
            "spawned_by_cwd": "/Users/x/code/footnote",
        }
    ]
    e = _apply_graph_defaults(entries)[0]
    assert e["source_node_id"] == "ab-origin01"
    assert e["spawned_by_session"] == "4ec8a08b"
    assert e["spawned_by_harness"] == "claude"
    assert e["spawned_by_cwd"] == "/Users/x/code/footnote"


def test_ac_edge_parent_edge_survives_save_reload(tmp_path, monkeypatch):
    """AC (x-30f6): source_node_id + spawned_by_* round-trip through locked_mutate_graph without loss."""
    g = _make_graph(tmp_path, [
        {
            "id": "ab-rt000001",
            "title": "Round-trip entry",
            "_status": "ready",
            "domain": "code",
            "project": "fno",
            "source_node_id": "ab-origin02",
            "spawned_by_session": "deadbeef",
            "spawned_by_harness": "claude",
            "spawned_by_cwd": "/Users/x/wt",
        }
    ])
    _patch_graph(monkeypatch, g)

    from fno.graph.store import read_graph, locked_mutate_graph

    def identity(es: list[dict]) -> list[dict]:
        return es

    locked_mutate_graph(g, identity)
    reloaded = read_graph(g)
    assert reloaded[0]["source_node_id"] == "ab-origin02"
    assert reloaded[0]["spawned_by_session"] == "deadbeef"
    assert reloaded[0]["spawned_by_harness"] == "claude"
    assert reloaded[0]["spawned_by_cwd"] == "/Users/x/wt"


def test_ac_entry_model_declares_parent_edge_fields():
    """AC (x-30f6): the Entry model carries the new fields as first-class (typed, default None)."""
    from fno.graph.types import Entry

    e = Entry(id="ab-model001", title="m")
    dumped = e.model_dump()
    for f in ("source_node_id", "spawned_by_session", "spawned_by_harness", "spawned_by_cwd"):
        assert f in dumped, f"{f} missing from Entry.model_dump()"
        assert dumped[f] is None


# ---------------------------------------------------------------------------
# x-30f6 Task 2.1 - ambient stamp at node birth (_session_provenance)
# ---------------------------------------------------------------------------


def _clear_session_env(monkeypatch):
    for var in (
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
    ):
        monkeypatch.delenv(var, raising=False)


def _write_manifest(cwd: Path, *, transcript_id: str, node_id: str, plan_path: str) -> None:
    (cwd / ".fno").mkdir(parents=True, exist_ok=True)
    (cwd / ".fno" / "target-state.md").write_text(
        "---\n"
        f'claude_transcript_id: {transcript_id}\n'
        f'plan_path: "{plan_path}"\n'
        "---\n"
        f"graph_node_id: {node_id}\n",
        encoding="utf-8",
    )


def test_ambient_hp_claude_session_node_and_plan(tmp_path, monkeypatch):
    """AC-HP (2.1): env session id + bound manifest -> all four fields stamped, no arg passed."""
    from fno.graph.cli import _session_provenance

    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-uuid-123")
    _write_manifest(tmp_path, transcript_id="sess-uuid-123",
                    node_id="ab-origin99", plan_path="internal/p/plan.md")

    prov = _session_provenance(str(tmp_path))
    assert prov["source_session_id"] == "sess-uuid-123"
    assert prov["source_harness"] == "claude"
    assert prov["source_cwd"] == str(tmp_path)
    assert prov["source_node_id"] == "ab-origin99"
    assert prov["source_plan_path"] == "internal/p/plan.md"


def test_ambient_plan_path_with_spaces_not_truncated(tmp_path, monkeypatch):
    """gemini review: a plan_path containing spaces is captured whole, not truncated."""
    from fno.graph.cli import _session_provenance

    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-space")
    _write_manifest(tmp_path, transcript_id="sess-space",
                    node_id="ab-spaced01", plan_path="internal/p/My Big Plan.md")

    prov = _session_provenance(str(tmp_path))
    assert prov["source_plan_path"] == "internal/p/My Big Plan.md"


def test_ambient_edge_no_env_all_none(tmp_path, monkeypatch):
    """AC-EDGE (2.1): absent env degrades to null, never raises."""
    from fno.graph.cli import _session_provenance

    _clear_session_env(monkeypatch)
    prov = _session_provenance(str(tmp_path))
    assert prov == {
        "source_session_id": None,
        "source_harness": None,
        "source_cwd": None,
        "source_node_id": None,
        "source_plan_path": None,
    }


def test_ambient_edge_session_without_manifest(tmp_path, monkeypatch):
    """AC-EDGE (2.1): env session set but no manifest -> session+harness only, node/plan null."""
    from fno.graph.cli import _session_provenance

    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-solo")
    prov = _session_provenance(str(tmp_path))
    assert prov["source_session_id"] == "sess-solo"
    assert prov["source_harness"] == "claude"
    assert prov["source_node_id"] is None
    assert prov["source_plan_path"] is None


def test_ambient_edge_ownership_mismatch_drops_node(tmp_path, monkeypatch):
    """AC-EDGE (2.1): a manifest owned by a DIFFERENT session never leaks its node."""
    from fno.graph.cli import _session_provenance

    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-mine")
    _write_manifest(tmp_path, transcript_id="sess-OTHER",
                    node_id="ab-notmine", plan_path="p.md")

    prov = _session_provenance(str(tmp_path))
    assert prov["source_session_id"] == "sess-mine"
    assert prov["source_node_id"] is None  # never guess on ownership mismatch
    assert prov["source_plan_path"] is None


def test_ambient_codex_degrades_to_session_no_manifest_read(tmp_path, monkeypatch):
    """AC-EDGE (2.1): codex session stamped + harness=codex; manifest read is claude-only."""
    from fno.graph.cli import _session_provenance

    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CODEX_SESSION_ID", "codex-abc")
    _write_manifest(tmp_path, transcript_id="codex-abc",
                    node_id="ab-codexnode", plan_path="p.md")

    prov = _session_provenance(str(tmp_path))
    assert prov["source_session_id"] == "codex-abc"
    assert prov["source_harness"] == "codex"
    # node/plan resolution is claude-only (the proven resolver lane); degrade.
    assert prov["source_node_id"] is None
    assert prov["source_plan_path"] is None


def test_ambient_codex_thread_precedes_claude_and_skips_manifest(tmp_path, monkeypatch):
    from fno.graph.cli import _session_provenance

    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-abc")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "claude-abc")
    _write_manifest(tmp_path, transcript_id="claude-abc",
                    node_id="ab-claudenode", plan_path="p.md")

    prov = _session_provenance(str(tmp_path))
    assert prov["source_session_id"] == "thread-abc"
    assert prov["source_harness"] == "codex"
    assert prov["source_node_id"] is None
    assert prov["source_plan_path"] is None


def test_build_backlog_node_stamps_ambient(monkeypatch):
    """AC-HP (2.1): _build_backlog_node merges ambient provenance with no caller arg."""
    import fno.graph.cli as gcli

    monkeypatch.setattr(gcli, "_session_provenance", lambda cwd=None: {
        "source_session_id": "S",
        "source_harness": "claude",
        "source_cwd": "/wt/sess",
        "source_node_id": "ab-parent01",
        "source_plan_path": "plan.md",
    })
    node = gcli._build_backlog_node(title="child")
    assert node["source_session_id"] == "S"
    assert node["source_harness"] == "claude"
    assert node["source_cwd"] == "/wt/sess"
    assert node["source_node_id"] == "ab-parent01"
    assert node["source_plan_path"] == "plan.md"


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
