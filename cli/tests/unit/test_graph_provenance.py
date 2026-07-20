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


def test_us6_harness_stamp_written_and_cleared(tmp_path, monkeypatch):
    """US6: `update --locked-by X --locked-by-harness ...` stamps the holder's
    provider + harness UUID over a stale owner; --locked-by null clears all three."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [{
        "id": "ab-harnes01", "title": "t", "plan_path": "p.md",
        "session_id": "stale-owner", "claimed_at": "2020-01-01T00:00:00Z",
    }])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)

    r = CliRunner().invoke(C.cli, [
        "update", "ab-harnes01", "--locked-by", "new-owner",
        "--locked-by-harness", "claude", "--locked-by-harness-session", "uuid-9",
    ])
    assert r.exit_code == 0, r.output
    node = read_graph(g)[0]
    assert node["locked_by"] == "new-owner"          # stale owner overwritten
    assert node["session_id"] == "new-owner"          # mirror synced
    assert node["locked_by_harness"] == "claude"
    assert node["locked_by_harness_session"] == "uuid-9"
    assert node["_status"] == "in_progress"

    r2 = CliRunner().invoke(C.cli, ["update", "ab-harnes01", "--locked-by", "null"])
    assert r2.exit_code == 0, r2.output
    cleared = read_graph(g)[0]
    assert cleared["locked_by"] is None
    assert cleared["locked_by_harness"] is None
    assert cleared["locked_by_harness_session"] is None
    assert cleared["_status"] == "ready"


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


# ---------------------------------------------------------------------------
# x-b6e4 - phase-tagged session provenance (sessions list + session add)
# ---------------------------------------------------------------------------


def test_sessions_defaults_empty_for_legacy_entry():
    """AC (x-b6e4): a legacy node with no `sessions` key reads as an empty list."""
    from fno.graph.store import _apply_graph_defaults

    e = _apply_graph_defaults([{"id": "ab-nosess01", "title": "old"}])[0]
    assert e["sessions"] == []


def test_sessions_existing_list_preserved():
    """AC (x-b6e4): setdefault is a no-op when sessions is already populated."""
    from fno.graph.store import _apply_graph_defaults

    rec = {"phase": "think", "harness": "claude", "session_id": "S", "at": "2026-07-12T00:00:00Z"}
    e = _apply_graph_defaults([{"id": "ab-hassess1", "sessions": [rec]}])[0]
    assert e["sessions"] == [rec]


def test_entry_model_declares_sessions_field():
    """AC (x-b6e4): Entry carries `sessions` as a first-class list, default empty."""
    from fno.graph.types import Entry

    dumped = Entry(id="ab-model002", title="m").model_dump()
    assert dumped["sessions"] == []


def test_sessions_survive_save_reload(tmp_path, monkeypatch):
    """AC (x-b6e4): sessions round-trip through locked_mutate_graph unchanged + in order."""
    rec_a = {"phase": "think", "harness": "claude", "session_id": "S1", "at": "2026-07-12T01:00:00Z"}
    rec_b = {"phase": "blueprint", "harness": "claude", "session_id": "S1", "at": "2026-07-12T02:00:00Z"}
    g = _make_graph(tmp_path, [{"id": "ab-rtsess01", "title": "rt", "sessions": [rec_a, rec_b]}])
    _patch_graph(monkeypatch, g)

    from fno.graph.store import read_graph, locked_mutate_graph

    locked_mutate_graph(g, lambda es: es)
    reloaded = read_graph(g)
    assert reloaded[0]["sessions"] == [rec_a, rec_b]


# -- append_session_record store primitive --


def _node_sessions(g, node_id):
    from fno.graph.store import read_graph
    for e in read_graph(g):
        if e["id"] == node_id:
            return e.get("sessions", [])
    return None


def test_append_session_record_appends(tmp_path, monkeypatch):
    """AC1-HP: append a think entry; found+added true, row present with all four keys."""
    g = _make_graph(tmp_path, [{"id": "ab-add00001", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    found, added = append_session_record(
        g, "ab-add00001", phase="think", harness="claude",
        session_id="S", at="2026-07-12T03:00:00Z",
    )
    assert (found, added) == (True, True)
    rows = _node_sessions(g, "ab-add00001")
    assert rows == [{"phase": "think", "harness": "claude",
                     "session_id": "S", "at": "2026-07-12T03:00:00Z"}]


def test_append_session_record_same_session_two_phases(tmp_path, monkeypatch):
    """AC1-HP: think + blueprint in one session -> two independently-queryable entries."""
    g = _make_graph(tmp_path, [{"id": "ab-add00002", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    append_session_record(g, "ab-add00002", phase="think", harness="claude", session_id="S")
    append_session_record(g, "ab-add00002", phase="blueprint", harness="claude", session_id="S")
    rows = _node_sessions(g, "ab-add00002")
    phases = [r["phase"] for r in rows]
    assert phases == ["think", "blueprint"]
    assert all(r["session_id"] == "S" for r in rows)


def test_append_session_record_dedup_preserves_first_at(tmp_path, monkeypatch):
    """AC5-FR: a retry of the same (phase,harness,session) is added:false, first `at` kept."""
    g = _make_graph(tmp_path, [{"id": "ab-add00003", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    append_session_record(g, "ab-add00003", phase="do", harness="codex",
                          session_id="S", at="2026-07-12T04:00:00Z")
    found, added = append_session_record(g, "ab-add00003", phase="do", harness="codex",
                                         session_id="S", at="2026-07-12T05:00:00Z")
    assert (found, added) == (True, False)
    rows = _node_sessions(g, "ab-add00003")
    assert len(rows) == 1
    assert rows[0]["at"] == "2026-07-12T04:00:00Z"  # first observation owns `at`


def test_append_session_record_takeover_same_phase_diff_session(tmp_path, monkeypatch):
    """AC4-EDGE: two distinct sessions ship -> both `ship` rows remain."""
    g = _make_graph(tmp_path, [{"id": "ab-add00004", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    append_session_record(g, "ab-add00004", phase="ship", harness="claude", session_id="S1")
    append_session_record(g, "ab-add00004", phase="ship", harness="claude", session_id="S2")
    rows = _node_sessions(g, "ab-add00004")
    assert [r["session_id"] for r in rows] == ["S1", "S2"]


def test_append_session_record_unknown_node(tmp_path, monkeypatch):
    """found=False when the node is absent; no mutation."""
    g = _make_graph(tmp_path, [{"id": "ab-add00005", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    found, added = append_session_record(g, "ab-missing", phase="do",
                                         harness="claude", session_id="S")
    assert (found, added) == (False, False)


@pytest.mark.parametrize("phase", ["review", "plan", "", "DO"])
def test_append_session_record_rejects_bad_phase(tmp_path, monkeypatch, phase):
    g = _make_graph(tmp_path, [{"id": "ab-add00006", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    with pytest.raises(ValueError):
        append_session_record(g, "ab-add00006", phase=phase, harness="claude", session_id="S")


@pytest.mark.parametrize("harness,sid", [("", "S"), ("claude", ""), ("  ", "S"), ("claude", "  ")])
def test_append_session_record_rejects_empty_identity(tmp_path, monkeypatch, harness, sid):
    g = _make_graph(tmp_path, [{"id": "ab-add00007", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    with pytest.raises(ValueError):
        append_session_record(g, "ab-add00007", phase="do", harness=harness, session_id=sid)


@pytest.mark.parametrize("bad_at", [
    "not-a-timestamp",
    "2026-07-12",              # date-only, no time/zone
    "2026-07-12T03:00:00",     # naive (no tz)
    "2026-07-12T03:00:00-07:00",  # non-UTC offset
])
def test_append_session_record_rejects_non_utc_at(tmp_path, monkeypatch, bad_at):
    """The UTC contract rejects date-only, naive, and non-UTC-offset timestamps."""
    g = _make_graph(tmp_path, [{"id": "ab-add00008", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record

    with pytest.raises(ValueError):
        append_session_record(g, "ab-add00008", phase="do", harness="claude",
                              session_id="S", at=bad_at)


@pytest.mark.parametrize("good_at,stored", [
    ("2026-07-12T03:00:00Z", "2026-07-12T03:00:00Z"),
    ("2026-07-12T03:00:00+00:00", "2026-07-12T03:00:00Z"),  # normalized to Z
])
def test_append_session_record_accepts_utc_at(tmp_path, monkeypatch, good_at, stored):
    """A tz-aware UTC timestamp is accepted and normalized to the canonical `...Z` form."""
    g = _make_graph(tmp_path, [{"id": "ab-add00009", "title": "t"}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import append_session_record, read_graph

    append_session_record(g, "ab-add00009", phase="do", harness="claude",
                          session_id="S", at=good_at)
    assert read_graph(g)[0]["sessions"][0]["at"] == stored


# -- stamp_session_for_pr: resolve the unique PR-linked node (Locked Decision 9) --


def test_stamp_session_for_pr_unique(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [
        {"id": "ab-pr000001", "title": "t", "pr_number": 500},
        {"id": "ab-pr000002", "title": "u", "pr_number": 501},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(g, 500, phase="ship",
                                           harness="claude", session_id="S")
    assert (node_id, status) == ("ab-pr000001", "added")
    assert _node_sessions(g, "ab-pr000001")[0]["phase"] == "ship"


def test_stamp_session_for_pr_matches_additional_prs(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [
        {"id": "ab-pr000003", "title": "t", "pr_number": 600,
         "additional_prs": [{"number": 601, "url": None, "note": None}]},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(g, 601, phase="ship",
                                           harness="claude", session_id="S")
    assert (node_id, status) == ("ab-pr000003", "added")


def test_stamp_session_for_pr_no_node(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [{"id": "ab-pr000004", "title": "t", "pr_number": 700}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    assert stamp_session_for_pr(g, 999, phase="ship",
                                harness="claude", session_id="S") == (None, "no-node")


def test_stamp_session_for_pr_ambiguous_never_fans_out(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [
        {"id": "ab-pr000005", "title": "t", "pr_number": 800},
        {"id": "ab-pr000006", "title": "u", "pr_number": 800},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(g, 800, phase="ship",
                                           harness="claude", session_id="S")
    assert (node_id, status) == (None, "ambiguous")
    # no mutation on either node
    assert _node_sessions(g, "ab-pr000005") == []
    assert _node_sessions(g, "ab-pr000006") == []


def test_stamp_session_for_pr_duplicate(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [{"id": "ab-pr000007", "title": "t", "pr_number": 900}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    stamp_session_for_pr(g, 900, phase="ship", harness="claude", session_id="S")
    node_id, status = stamp_session_for_pr(g, 900, phase="ship",
                                           harness="claude", session_id="S")
    assert (node_id, status) == ("ab-pr000007", "duplicate")
    assert len(_node_sessions(g, "ab-pr000007")) == 1


# -- x-d5f9: repo-scoped resolution (pr_number collides across repos) --


def _colliding_repo_graph(tmp_path):
    """Two nodes carry pr_number 388 with pr_url in DIFFERENT repos (the live bug)."""
    return _make_graph(tmp_path, [
        {"id": "x-foot0001", "title": "footnote node", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/footnote/pull/388"},
        {"id": "ab-abil0001", "title": "abilities node", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/abilities/pull/388"},
    ])


def test_stamp_repo_scoped_picks_right_node(tmp_path, monkeypatch):
    """AC1-HP: repo-scoped resolution stamps exactly the same-repo node."""
    g = _colliding_repo_graph(tmp_path)
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(
        g, 388, phase="ship", harness="claude", session_id="S",
        repo="bllshttng/footnote",
    )
    assert (node_id, status) == ("x-foot0001", "added")
    assert _node_sessions(g, "x-foot0001")[0]["phase"] == "ship"
    assert _node_sessions(g, "ab-abil0001") == []  # untouched


def test_stamp_repo_scoped_no_match_skips(tmp_path, monkeypatch):
    """AC1-ERR: no node's pr_url matches the repo -> no-node, nothing mutated."""
    g = _colliding_repo_graph(tmp_path)
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(
        g, 388, phase="ship", harness="claude", session_id="S",
        repo="bllshttng/some-other-repo",
    )
    assert (node_id, status) == (None, "no-node")
    assert _node_sessions(g, "x-foot0001") == []
    assert _node_sessions(g, "ab-abil0001") == []


def test_stamp_repo_none_falls_back_to_bare_number(tmp_path, monkeypatch):
    """AC1-FR: repo=None on a lone node keeps today's bare-pr_number match."""
    g = _make_graph(tmp_path, [
        {"id": "x-lone0001", "title": "t", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/footnote/pull/388"},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(g, 388, phase="ship",
                                           harness="claude", session_id="S")
    assert (node_id, status) == ("x-lone0001", "added")


def test_stamp_repo_scoped_same_repo_multi_stays_ambiguous(tmp_path, monkeypatch):
    """AC1-EDGE: two nodes in the SAME repo for the same PR -> ambiguous, no stamp."""
    g = _make_graph(tmp_path, [
        {"id": "x-same0001", "title": "t", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/footnote/pull/388"},
        {"id": "x-same0002", "title": "u", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/footnote/pull/388"},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(
        g, 388, phase="ship", harness="claude", session_id="S",
        repo="bllshttng/footnote",
    )
    assert (node_id, status) == (None, "ambiguous")
    assert _node_sessions(g, "x-same0001") == []
    assert _node_sessions(g, "x-same0002") == []


def test_stamp_repo_scoped_excludes_urlless_legacy_node(tmp_path, monkeypatch):
    """Failure Modes/Boundaries: a node with pr_number but no pr_url is unattributable."""
    g = _make_graph(tmp_path, [
        {"id": "x-nourl0001", "title": "t", "pr_number": 388, "pr_url": None},
        {"id": "x-foot0001", "title": "u", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/footnote/pull/388"},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(
        g, 388, phase="ship", harness="claude", session_id="S",
        repo="bllshttng/footnote",
    )
    assert (node_id, status) == ("x-foot0001", "added")  # urlless node never matches


def test_stamp_repo_scoped_matches_additional_prs_url(tmp_path, monkeypatch):
    """Boundaries: a repo match on an additional_prs url, not only the primary."""
    g = _make_graph(tmp_path, [
        {"id": "x-addl0001", "title": "t", "pr_number": 12,
         "pr_url": "https://github.com/bllshttng/footnote/pull/12",
         "additional_prs": [
             {"number": 388, "url": "https://github.com/bllshttng/abilities/pull/388", "note": None}
         ]},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(
        g, 388, phase="ship", harness="claude", session_id="S",
        repo="bllshttng/abilities",
    )
    assert (node_id, status) == ("x-addl0001", "added")


def test_stamp_repo_scoped_pull_number_boundary(tmp_path, monkeypatch):
    """A /pull/388 request must not match a node whose url is /pull/3880."""
    g = _make_graph(tmp_path, [
        {"id": "x-bound001", "title": "t", "pr_number": 3880,
         "pr_url": "https://github.com/bllshttng/footnote/pull/3880"},
    ])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    assert stamp_session_for_pr(
        g, 388, phase="ship", harness="claude", session_id="S",
        repo="bllshttng/footnote",
    ) == (None, "no-node")


@pytest.mark.parametrize("url,repo", [
    ("https://github.com/bllshttng/footnote/pull/388/", "bllshttng/footnote"),      # trailing slash
    ("https://github.com/bllshttng/footnote/pull/388?w=1", "bllshttng/footnote"),   # query
    ("https://github.com/bllshttng/footnote/pull/388#issue-1", "bllshttng/footnote"),  # fragment
    ("https://github.com/Bllshttng/Footnote/pull/388", "bllshttng/footnote"),       # casing mismatch
])
def test_stamp_repo_scoped_matches_url_variants(tmp_path, monkeypatch, url, repo):
    """gemini review: trailing slash / query / fragment / casing must not false-negative."""
    g = _make_graph(tmp_path, [{"id": "x-var00001", "title": "t", "pr_number": 388, "pr_url": url}])
    _patch_graph(monkeypatch, g)
    from fno.graph.store import stamp_session_for_pr

    node_id, status = stamp_session_for_pr(
        g, 388, phase="ship", harness="claude", session_id="S", repo=repo,
    )
    assert (node_id, status) == ("x-var00001", "added")


# -- `fno backlog session add` CLI (reuses _clear_session_env above) --


def test_cli_session_add_pr_mode_resolves_node(tmp_path, monkeypatch):
    """`session add --pr <n>` resolves the unique PR-linked node and stamps it."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [{"id": "ab-prcli001", "title": "t", "pr_number": 1200}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _stub_slug(monkeypatch, "bllshttng/footnote")
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-pr")

    r = CliRunner().invoke(C.cli, ["session", "add", "--pr-number", "1200", "--phase", "ship", "--json"])
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["node_id"] == "ab-prcli001" and out["added"] is True
    assert read_graph(g)[0]["sessions"][0]["phase"] == "ship"


def test_cli_session_add_pr_repo_scopes_resolution(tmp_path, monkeypatch):
    """AC1-HP (CLI): --repo disambiguates a pr_number that collides across repos."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [
        {"id": "x-clifoot1", "title": "t", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/footnote/pull/388"},
        {"id": "ab-cliabil1", "title": "u", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/abilities/pull/388"},
    ])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-pr")

    r = CliRunner().invoke(C.cli, [
        "session", "add", "--pr-number", "388",
        "--repo", "bllshttng/footnote", "--phase", "ship", "--json",
    ])
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["node_id"] == "x-clifoot1"
    by_id = {e["id"]: e for e in read_graph(g)}
    assert by_id["ab-cliabil1"].get("sessions", []) == []  # other repo untouched


def _stub_slug(monkeypatch, slug):
    """Pin the auto-resolved repo slug (x-f47f US1) so tests never shell out."""
    import fno.graph._reconcile as R
    monkeypatch.setattr(R, "resolve_current_repo_slug", lambda *a, **k: slug)


# -- resolve_current_repo_slug (x-f47f US1: git origin first, gh fallback) --


@pytest.mark.parametrize("url,want", [
    ("git@github.com:bllshttng/footnote.git\n", "bllshttng/footnote"),
    ("https://github.com/bllshttng/footnote.git\n", "bllshttng/footnote"),
    ("https://github.com/bllshttng/footnote\n", "bllshttng/footnote"),
])
def test_resolve_repo_slug_from_git_origin(url, want):
    from fno.graph._reconcile import resolve_current_repo_slug

    calls = []

    def runner(argv, cwd):
        calls.append(argv[0])
        return (0, url) if argv[0] == "git" else (0, "wrong/repo\n")

    assert resolve_current_repo_slug(runner=runner) == want
    assert calls == ["git"]  # gh is never consulted when git answers


def test_resolve_repo_slug_falls_back_to_gh():
    """No origin remote (or a non-GitHub one) -> gh repo view answers."""
    from fno.graph._reconcile import resolve_current_repo_slug

    def runner(argv, cwd):
        return (128, "") if argv[0] == "git" else (0, "bllshttng/footnote\n")

    assert resolve_current_repo_slug(runner=runner) == "bllshttng/footnote"


def test_resolve_repo_slug_none_when_both_fail():
    """gh missing/unauthed AND no git origin -> None (caller degrades, never guesses)."""
    from fno.graph._reconcile import resolve_current_repo_slug

    assert resolve_current_repo_slug(runner=lambda argv, cwd: (127, "")) is None


def test_cli_session_add_pr_ambiguous_skips_and_exits_zero(tmp_path, monkeypatch):
    """AC3-ERR (x-f47f): an unresolvable repo + cross-repo collision is a SKIP.

    Exit 0 with an ambiguous warning naming both candidates: refusing to guess is
    the designed outcome, so the caller's Failures line must not log it as a
    failure. Nothing is stamped.
    """
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [
        {"id": "ab-prcli002", "title": "t", "pr_number": 1300},
        {"id": "ab-prcli003", "title": "u", "pr_number": 1300},
    ])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _stub_slug(monkeypatch, None)
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-pr")

    r = CliRunner().invoke(C.cli, ["session", "add", "--pr-number", "1300", "--phase", "ship"])
    assert r.exit_code == 0, r.output
    assert "ambiguous" in r.output and "1300" in r.output
    assert "ab-prcli002" in r.output and "ab-prcli003" in r.output
    assert all(e.get("sessions", []) == [] for e in read_graph(g))


def test_cli_session_add_pr_auto_resolves_repo_slug(tmp_path, monkeypatch):
    """US1: no --repo needed - the verb resolves this checkout's slug itself."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [
        {"id": "x-autofoot", "title": "t", "pr_number": 480,
         "pr_url": "https://github.com/bllshttng/footnote/pull/480"},
        {"id": "ab-autoabil", "title": "u", "pr_number": 480,
         "pr_url": "https://github.com/bllshttng/abilities/pull/480"},
    ])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _stub_slug(monkeypatch, "bllshttng/footnote")
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-pr")

    r = CliRunner().invoke(C.cli, [
        "session", "add", "--pr-number", "480", "--phase", "ship", "--json",
    ])
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["node_id"] == "x-autofoot"
    by_id = {e["id"]: e for e in read_graph(g)}
    assert by_id["ab-autoabil"].get("sessions", []) == []


def test_cli_session_add_pr_auto_slug_narrows_never_excludes(tmp_path, monkeypatch):
    """A pr_number-only node (no pr_url) is unattributable to a repo, so the
    AUTO-resolved slug must fall back to the bare-number match rather than
    silently stamping nothing - the invisible no-op this change exists to kill."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [{"id": "ab-legacy01", "title": "t", "pr_number": 1500}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _stub_slug(monkeypatch, "bllshttng/footnote")
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-pr")

    r = CliRunner().invoke(C.cli, ["session", "add", "--pr-number", "1500", "--phase", "ship"])
    assert r.exit_code == 0, r.output
    assert read_graph(g)[0]["sessions"][0]["phase"] == "ship"


def test_cli_session_add_explicit_repo_stays_a_hard_filter(tmp_path, monkeypatch):
    """An EXPLICIT --repo must not get the narrow-never-exclude fallback: the
    caller asserted the repo, so a non-match is a skip, not a bare-number stamp."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [{"id": "ab-legacy02", "title": "t", "pr_number": 1600}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-pr")

    r = CliRunner().invoke(C.cli, [
        "session", "add", "--pr-number", "1600", "--repo", "bllshttng/footnote",
        "--phase", "ship",
    ])
    assert r.exit_code == 0, r.output
    assert "no-node" in r.output
    assert read_graph(g)[0].get("sessions", []) == []


def test_cli_session_add_requires_node_or_pr(tmp_path, monkeypatch):
    """Neither NODE nor --pr -> usage error; both -> usage error."""
    from typer.testing import CliRunner
    import fno.graph.cli as C

    g = _make_graph(tmp_path, [{"id": "ab-prcli004", "title": "t", "pr_number": 1400}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-pr")

    assert CliRunner().invoke(C.cli, ["session", "add", "--phase", "ship"]).exit_code != 0
    assert CliRunner().invoke(
        C.cli, ["session", "add", "ab-prcli004", "--pr-number", "1400", "--phase", "ship"]
    ).exit_code != 0


def test_cli_session_add_uses_ambient_identity(tmp_path, monkeypatch):
    """AC1-HP: `session add` defaults harness+session from ambient env; exits 0."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [{"id": "ab-cli00001", "title": "t"}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-cli-1")

    r = CliRunner().invoke(C.cli, ["session", "add", "ab-cli00001", "--phase", "think"])
    assert r.exit_code == 0, r.output
    rows = read_graph(g)[0]["sessions"]
    assert rows == [{"phase": "think", "harness": "claude",
                     "session_id": "sess-cli-1", "at": rows[0]["at"]}]
    assert rows[0]["at"]  # a timestamp was stamped


def test_cli_session_add_duplicate_exits_zero_added_false(tmp_path, monkeypatch):
    """duplicate -> exit 0, JSON added:false."""
    from typer.testing import CliRunner
    import fno.graph.cli as C

    g = _make_graph(tmp_path, [{"id": "ab-cli00002", "title": "t"}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)

    args = ["session", "add", "ab-cli00002", "--phase", "do",
            "--harness", "codex", "--session-id", "S", "--json"]
    r1 = CliRunner().invoke(C.cli, args)
    assert r1.exit_code == 0, r1.output
    assert json.loads(r1.output)["added"] is True
    r2 = CliRunner().invoke(C.cli, args)
    assert r2.exit_code == 0, r2.output
    assert json.loads(r2.output)["added"] is False


def test_cli_session_add_missing_identity_exits_nonzero_no_mutation(tmp_path, monkeypatch):
    """AC2-ERR: no ambient identity + no explicit flags -> nonzero, no mutation, warns node+phase."""
    from typer.testing import CliRunner
    import fno.graph.cli as C
    from fno.graph.store import read_graph

    g = _make_graph(tmp_path, [{"id": "ab-cli00003", "title": "t"}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)

    r = CliRunner().invoke(C.cli, ["session", "add", "ab-cli00003", "--phase", "ship"])
    assert r.exit_code != 0
    assert "ab-cli00003" in r.output and "ship" in r.output
    assert read_graph(g)[0].get("sessions", []) == []


def test_cli_session_add_bad_phase_exits_nonzero(tmp_path, monkeypatch):
    from typer.testing import CliRunner
    import fno.graph.cli as C

    g = _make_graph(tmp_path, [{"id": "ab-cli00004", "title": "t"}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "S")

    r = CliRunner().invoke(C.cli, ["session", "add", "ab-cli00004", "--phase", "review"])
    assert r.exit_code != 0


def test_cli_session_add_unknown_node_exits_nonzero(tmp_path, monkeypatch):
    from typer.testing import CliRunner
    import fno.graph.cli as C

    g = _make_graph(tmp_path, [{"id": "ab-cli00005", "title": "t"}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setattr(C, "_graph_path", lambda: g)
    _clear_session_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "S")

    r = CliRunner().invoke(C.cli, ["session", "add", "ab-nope", "--phase", "do"])
    assert r.exit_code != 0
