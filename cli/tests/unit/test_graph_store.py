"""Unit tests for fno.graph.store - flock helpers, load/write, locked_mutate.

These tests are ported from tests/test_graph.py and target the extracted module.
They run the module functions directly (no subprocess) for speed.
"""
from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path

import pytest

from fno.graph.store import (
    GraphCorruptError,
    _acquire_flock,
    _apply_graph_defaults,
    append_session_record,
    _read_json,
    _release_flock,
    _write_json,
    locked_mutate_graph,
    read_graph,
)
from fno.graph.statuses import recompute_statuses, is_stale_lock


# -- helpers --


def _make_graph(tmp_path: Path, entries: list[dict]) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}) + "\n")
    return p


# -- tests --


def test_locked_by_normalized_from_legacy_session_id():
    """US3: a pre-rename node (session_id only) gets locked_by on load, mirrored."""
    e = {"id": "ab-11112222", "session_id": "sess-old", "plan_path": "p.md"}
    out = _apply_graph_defaults([e])[0]
    assert out["locked_by"] == "sess-old"
    assert out["session_id"] == "sess-old"  # mirror preserved


def test_locked_by_wins_when_both_present_and_differ():
    """US3: locked_by is canonical; a divergent session_id is overwritten."""
    e = {"id": "ab-11112223", "locked_by": "new-owner", "session_id": "stale"}
    out = _apply_graph_defaults([e])[0]
    assert out["locked_by"] == "new-owner"
    assert out["session_id"] == "new-owner"


def test_clearing_owner_clears_harness_stamp():
    """P2: any path that clears locked_by drops the harness stamp at normalize,
    so a re-claim can never route to a stale holder."""
    e = {
        "id": "ab-clr00001", "locked_by": "owner-1",
        "locked_by_harness": "claude", "locked_by_harness_session": "uuid-1",
    }
    # Simulate a clear path (defer/done/unclaim) that only nulls locked_by.
    e["locked_by"] = None
    out = _apply_graph_defaults([e])[0]
    assert out["locked_by"] is None
    assert out["session_id"] is None
    assert out["locked_by_harness"] is None
    assert out["locked_by_harness_session"] is None


def test_ac7_edge_mixed_version_round_trip(tmp_path):
    """AC7-EDGE: legacy node (session_id only) round-trips through a mutation
    with locked_by == original session_id and status still 'claimed'."""
    from datetime import datetime, timezone
    now = datetime.now(timezone.utc).isoformat()
    p = _make_graph(tmp_path, [{
        "id": "ab-7edge001", "session_id": "worker-7", "claimed_at": now,
        "plan_path": "p.md",
    }])
    # Mutate an unrelated field.
    def mutator(entries):
        entries[0]["details"] = "touched"
        return entries
    locked_mutate_graph(p, mutator)
    saved = json.loads(p.read_text())["entries"][0]
    assert saved["locked_by"] == "worker-7"
    assert saved["session_id"] == "worker-7"  # mirror written
    assert saved["status"] == "in_progress"


def test_ac1_hp_acquire_release_flock(tmp_path):
    """AC1-HP: acquire and release flock without error."""
    lock = tmp_path / "test.lock"
    fd = _acquire_flock(lock)
    assert fd >= 0
    _release_flock(fd)  # should not raise


def test_ac1_hp_read_json_missing_file(tmp_path):
    """AC1-HP: _read_json returns [] for missing file."""
    p = tmp_path / "nonexistent.json"
    result = _read_json(p)
    assert result == []


def test_ac1_hp_read_json_empty_entries(tmp_path):
    """AC1-HP: _read_json returns [] for file with empty entries."""
    p = tmp_path / "g.json"
    p.write_text(json.dumps({"entries": []}) + "\n")
    result = _read_json(p)
    assert result == []


def test_ac1_hp_read_json_valid_entries(tmp_path):
    """AC1-HP: _read_json returns entries list."""
    p = tmp_path / "g.json"
    p.write_text(json.dumps({"entries": [{"id": "ab-aabbccdd", "title": "X"}]}) + "\n")
    result = _read_json(p)
    assert len(result) == 1
    assert result[0]["id"] == "ab-aabbccdd"


def test_ac2_err_read_json_corrupt(tmp_path):
    """AC2-ERR: _read_json raises GraphCorruptError on invalid JSON."""
    p = tmp_path / "g.json"
    p.write_text("not json at all")
    with pytest.raises(GraphCorruptError):
        _read_json(p)


def test_ac1_hp_write_json_roundtrip(tmp_path):
    """AC1-HP: _write_json creates file, _read_json reads it back."""
    p = tmp_path / "g.json"
    entries = [{"id": "ab-11223344", "title": "Roundtrip"}]
    _write_json(entries, p)
    result = _read_json(p)
    assert result == entries


def test_ac1_hp_write_json_atomic(tmp_path):
    """AC1-HP: _write_json uses temp file + os.replace (no partial writes)."""
    p = tmp_path / "g.json"
    entries = [{"id": "ab-aaaabbbb"}]
    _write_json(entries, p)
    assert p.exists()
    # No .tmp file should linger
    assert list(tmp_path.glob("*.tmp")) == []


def test_ac1_hp_apply_graph_defaults():
    """AC1-HP: _apply_graph_defaults fills in expected fields."""
    entries = [{"id": "ab-12345678", "title": "T"}]
    result = _apply_graph_defaults(entries)
    e = result[0]
    assert e["priority"] == "p2"
    assert e["domain"] == "code"
    assert e["blocked_by"] == []
    assert e["status"] == "ready"
    assert e["cost_sessions"] == []


# -- Phase 01: schema extension (artifact_url, completion_note) --


def test_scenario1_lazy_migration_artifact_url_default(tmp_path):
    """Scenario 1 (HP): Legacy entry without artifact_url key gets None on read."""
    path = _make_graph(tmp_path, [{"id": "ab-legacy01", "title": "T"}])
    entries = read_graph(path)
    assert entries[0]["artifact_url"] is None


def test_scenario1_lazy_migration_completion_note_default(tmp_path):
    """Scenario 1 (HP): Legacy entry without completion_note key gets None on read."""
    path = _make_graph(tmp_path, [{"id": "ab-legacy02", "title": "T"}])
    entries = read_graph(path)
    assert entries[0]["completion_note"] is None


def test_scenario3_edge_preserves_shim_artifact_url(tmp_path):
    """Scenario 3 (EDGE): setdefault preserves pre-set shim values."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-shim0001", "title": "T", "artifact_url": "https://figma/foo"}],
    )
    entries = read_graph(path)
    assert entries[0]["artifact_url"] == "https://figma/foo"


def test_scenario3_edge_preserves_shim_completion_note(tmp_path):
    """Scenario 3 (EDGE): setdefault preserves pre-set completion_note."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-shim0002", "title": "T", "completion_note": "closed Q2"}],
    )
    entries = read_graph(path)
    assert entries[0]["completion_note"] == "closed Q2"


def test_ac1_hp_locked_mutate_graph(tmp_path):
    """AC1-HP: locked_mutate_graph reads, applies mutator, writes back."""
    path = tmp_path / "graph.json"

    def mutator(entries):
        entries.append({"id": "ab-newnode0", "title": "New"})
        return entries

    locked_mutate_graph(path, mutator)
    result = _read_json(path)
    assert any(e.get("id") == "ab-newnode0" for e in result)


def test_touched_at_stamped_on_curation_change(tmp_path):
    """Positive control (x-7dcb): a priority change DOES stamp touched_at.
    Without this, AC4-EDGE's negative case proves nothing - a guard that
    never fires and a guard that always fires both pass a cwd-only test."""
    path = _make_graph(tmp_path, [{"id": "ab-1", "title": "T", "priority": "p2"}])

    def mutator(entries):
        for e in entries:
            if e["id"] == "ab-1":
                e["priority"] = "p1"
        return entries

    locked_mutate_graph(path, mutator)
    result = _read_json(path)
    node = next(e for e in result if e["id"] == "ab-1")
    assert node.get("touched_at")


def test_touched_at_unchanged_on_non_curation_write(tmp_path):
    """AC4-EDGE: a mutator that changes only cwd (janitorial rescope) must
    never stamp touched_at - a curation-blind write freezing the drain
    forever is the failure this test exists to catch.

    ``status`` is set to the value recompute_statuses would independently
    derive for this shape (no plan_path/pr_number/completed_at/deferred_at
    -> "idea"), matching a real persisted node: on disk, status was already
    written by a prior recompute_statuses cycle, so this fixture's raw
    default of "ready" would spuriously look like a curation change on the
    very first mutation - a test-fixture artifact, not a real bug (caught by
    this test failing before the fixture was corrected)."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-1", "title": "T", "priority": "p2", "status": "idea", "touched_at": "2020-01-01T00:00:00+00:00"}],
    )

    def mutator(entries):
        for e in entries:
            if e["id"] == "ab-1":
                e["cwd"] = "/new/path"
        return entries

    locked_mutate_graph(path, mutator)
    result = _read_json(path)
    node = next(e for e in result if e["id"] == "ab-1")
    assert node.get("touched_at") == "2020-01-01T00:00:00+00:00"


def test_touched_at_null_on_new_node(tmp_path):
    """A node absent from the pre-mutator image is new: created_at already
    carries that date, so touched_at is left null rather than double-stamped."""
    path = _make_graph(tmp_path, [])

    def mutator(entries):
        entries.append({"id": "ab-brand-new", "title": "New", "priority": "p2"})
        return entries

    locked_mutate_graph(path, mutator)
    result = _read_json(path)
    node = next(e for e in result if e["id"] == "ab-brand-new")
    assert node.get("touched_at") is None


def test_touched_at_unchanged_on_blocked_node_unrelated_write(tmp_path):
    """Regression (x-7dcb): a blocked node's read-time readiness overlay
    (`_apply_readiness_overlay`, applied by every `_apply_graph_defaults`
    call) stamps `status: "blocked"` into the pre-mutator snapshot, but
    `recompute_statuses` never derives "blocked" - so an unguarded
    comparison would misread EVERY mutation on a blocked node as a status
    change, even one that only edits an unrelated field. The blocker here
    stays unresolved across the mutation, so the overlay is unchanged too;
    touched_at must not move."""
    path = _make_graph(
        tmp_path,
        [
            {"id": "ab-blocker", "title": "Blocker", "status": "ready"},
            {
                "id": "ab-blocked",
                "title": "Blocked",
                "status": "idea",
                "blocked_by": ["ab-blocker"],
                "touched_at": "2020-01-01T00:00:00+00:00",
            },
        ],
    )

    def mutator(entries):
        for e in entries:
            if e["id"] == "ab-blocked":
                e["details"] = "unrelated edit"
        return entries

    locked_mutate_graph(path, mutator)
    result = _read_json(path)
    blocked = next(e for e in result if e["id"] == "ab-blocked")
    assert blocked.get("touched_at") == "2020-01-01T00:00:00+00:00"


def test_mutate_fail_open_when_vault_root_raises(tmp_path, monkeypatch):
    """A malformed settings file that makes vault_root() raise must not crash a
    graph mutation (Codex P2 on PR #430): graph.json is already written, so the
    Obsidian-gating decision falls open to no-scaffolding."""
    import fno.paths as paths_mod

    def boom():
        raise RuntimeError("malformed settings")

    monkeypatch.setattr(paths_mod, "vault_root", boom)
    path = tmp_path / "graph.json"

    def mutator(entries):
        entries.append({"id": "ab-failopen", "title": "FailOpen"})
        return entries

    # Must not raise despite vault_root() blowing up.
    locked_mutate_graph(path, mutator)
    result = _read_json(path)
    assert any(e.get("id") == "ab-failopen" for e in result)
    # graph.md rendered next to the json, fail-open without Obsidian frontmatter.
    md = (tmp_path / "graph.md").read_text()
    assert "kanban-plugin: board" not in md


def test_regression_mutate_renders_siblings_not_global(tmp_path, monkeypatch):
    """Regression: locked_mutate_graph renders graph.html/.md next to the
    graph.json it mutated, never the global ~/.fno targets.

    Guards the board-server bug where running the test suite clobbered the
    real ~/.fno/graph.html (served by serve_board.py over Tailscale)
    with single-fixture-node renders. Simulate the global location via a
    monkeypatched state_dir; if the auto-render ever falls back to the global
    default again, the fake_home assertions below trip instead of polluting
    the developer's actual ~/.fno.
    """
    fake_home = tmp_path / "fake_home_fno"
    fake_home.mkdir()
    monkeypatch.setattr(
        "fno.graph._constants._state_dir", lambda: fake_home
    )

    graph_dir = tmp_path / "work"
    graph_dir.mkdir()
    path = graph_dir / "graph.json"

    def mutator(entries):
        entries.append({"id": "ab-sibling1", "title": "Sib"})
        return entries

    locked_mutate_graph(path, mutator)

    # Renders land next to the mutated graph.json.
    assert (graph_dir / "graph.html").exists()
    assert (graph_dir / "graph.md").exists()
    # The (simulated) global location is never written.
    assert not (fake_home / "graph.html").exists()


def test_canonical_graph_renders_to_board_targets(tmp_path, monkeypatch):
    """Mutating the canonical graph.json renders to GRAPH_HTML/GRAPH_MD (what
    `fno backlog view` and serve_board.py read), not graph.json's siblings.

    Covers the config.paths.graph_json override case: when the configured
    graph.json lives outside state_dir, the board targets stay in state_dir so
    the served/opened board still reflects mutations.
    """
    import fno.graph._constants as gc

    state_dir = tmp_path / "state"
    state_dir.mkdir()
    custom_dir = tmp_path / "custom"
    custom_dir.mkdir()
    graph_json = custom_dir / "graph.json"  # graph_json outside state_dir
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_json)
    monkeypatch.setattr(gc, "GRAPH_HTML", state_dir / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_MD", state_dir / "graph.md")

    def mutator(entries):
        entries.append({"id": "ab-canon01", "title": "Canon"})
        return entries

    locked_mutate_graph(graph_json, mutator)

    # Board targets (state_dir) get the render, not graph.json's siblings.
    assert (state_dir / "graph.html").exists()
    assert (state_dir / "graph.md").exists()
    assert not (custom_dir / "graph.html").exists()


def test_canonical_auto_render_keeps_archive_only_rows(tmp_path, monkeypatch):
    """A mutation cannot clobber the private served board back to live-only."""
    import fno.graph._constants as gc

    state_dir = tmp_path / "state"
    state_dir.mkdir()
    graph_json = state_dir / "graph.json"
    archive_json = state_dir / "graph-archive.json"
    archive_json.write_text(
        json.dumps({"entries": [
            {"id": "ab-archive1", "title": "ARCHIVE-AUTO-RENDER-MARKER",
             "status": "done", "project": "fno"},
        ]}),
        encoding="utf-8",
    )
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_json)
    monkeypatch.setattr(gc, "GRAPH_HTML", state_dir / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_MD", state_dir / "graph.md")
    monkeypatch.setattr("fno.paths.graph_archive_json", lambda: archive_json)

    locked_mutate_graph(
        graph_json,
        lambda entries: [*entries, {"id": "ab-live0001", "title": "live"}],
    )

    assert "ARCHIVE-AUTO-RENDER-MARKER" in (state_dir / "graph.html").read_text()


def test_ac1_hp_read_graph_returns_with_defaults(tmp_path):
    """AC1-HP: read_graph applies defaults to entries."""
    path = _make_graph(tmp_path, [{"id": "ab-12341234", "title": "T"}])
    entries = read_graph(path)
    assert len(entries) == 1
    assert entries[0]["priority"] == "p2"


def test_ac2_err_read_graph_corrupt_returns_empty(tmp_path):
    """AC2-ERR: read_graph returns [] on corruption (does not raise)."""
    path = tmp_path / "corrupt.json"
    path.write_text("{ INVALID JSON }")
    entries = read_graph(path)
    assert entries == []


def test_legacy_underscore_status_key_migrates_on_read(tmp_path):
    """A pre-rename row carries `_status`; read_graph folds it into `status`."""
    path = _make_graph(
        tmp_path, [{"id": "ab-12341234", "title": "T", "_status": "claimed"}]
    )
    entry = read_graph(path)[0]
    assert "_status" not in entry
    # STATUS_MIGRATION still applies after the key fold.
    assert entry["status"] == "in_progress"


def _ready_plan_entry(tmp_path: Path, node_id: str = "ab-open0001") -> tuple[Path, dict]:
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\n---\n", encoding="utf-8")
    return plan, {
        "id": node_id,
        "cwd": str(tmp_path),
        "plan_path": plan.name,
        "sessions": [],
    }


def test_open_do_row_persists_in_progress_and_closed_row_demotes(tmp_path):
    """AC1/AC2: the open do row is the stored progress projection."""
    _plan, entry = _ready_plan_entry(tmp_path)
    path = _make_graph(tmp_path, [entry])

    found, added = append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="session-open",
        started_at="2026-08-20T00:00:00Z",
    )
    assert (found, added) == (True, True)
    saved = json.loads(path.read_text())["entries"][0]
    assert saved["status"] == "in_progress"

    found, added = append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="session-open",
        ended_at="2026-08-20T00:01:00Z",
    )
    assert (found, added) == (True, False)
    saved = json.loads(path.read_text())["entries"][0]
    assert saved["status"] == "ready"
    assert saved["sessions"][0]["ended_at"] == "2026-08-20T00:01:00Z"


def test_two_open_do_rows_keep_progress_until_last_row_closes(tmp_path):
    """AC4: reaping/closing one concurrent session keeps progress stored."""
    _plan, entry = _ready_plan_entry(tmp_path, "ab-open0002")
    path = _make_graph(tmp_path, [entry])
    for session_id in ("session-one", "session-two"):
        append_session_record(
            path,
            entry["id"],
            phase="do",
            harness="codex",
            session_id=session_id,
            started_at="2026-08-20T00:00:00Z",
        )

    append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="session-one",
        ended_at="2026-08-20T00:01:00Z",
    )
    assert json.loads(path.read_text())["entries"][0]["status"] == "in_progress"

    append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="session-two",
        ended_at="2026-08-20T00:02:00Z",
    )
    assert json.loads(path.read_text())["entries"][0]["status"] == "ready"


def test_reap_open_session_record_removes_exact_open_row_with_readback(tmp_path):
    """AC3/AC4: observer reaping removes one exact open row and settles status."""
    _plan, entry = _ready_plan_entry(tmp_path, "ab-reap0001")
    path = _make_graph(tmp_path, [entry])
    for session_id in ("dead-session", "live-session"):
        append_session_record(
            path,
            entry["id"],
            phase="do",
            harness="codex",
            session_id=session_id,
            started_at="2026-08-20T00:00:00Z",
        )

    from fno.graph import store as graph_store

    reap_open_session_record = getattr(graph_store, "reap_open_session_record", None)
    assert callable(reap_open_session_record)
    result = reap_open_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="dead-session",
    )

    assert result == {
        "found": True,
        "settled": True,
        "row_removed": True,
        # x-4342: the do path removes; only non-do phases close by filling.
        "row_closed": False,
        "status_before": "in_progress",
        "status_after": "in_progress",
        "remaining_open_do": 1,
    }
    rows = json.loads(path.read_text())["entries"][0]["sessions"]
    assert [(r["harness"], r["session_id"]) for r in rows] == [("codex", "live-session")]


def test_reap_open_session_record_does_not_remove_closed_row(tmp_path):
    """AC3: observer reap is idempotent and preserves closed provenance."""
    _plan, entry = _ready_plan_entry(tmp_path, "ab-reap0002")
    path = _make_graph(tmp_path, [entry])
    append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="closed-session",
        started_at="2026-08-20T00:00:00Z",
        ended_at="2026-08-20T00:01:00Z",
    )

    from fno.graph import store as graph_store

    reap_open_session_record = getattr(graph_store, "reap_open_session_record", None)
    assert callable(reap_open_session_record)
    result = reap_open_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="closed-session",
    )

    assert result["settled"] is True
    assert result["row_removed"] is False
    assert result["remaining_open_do"] == 0
    assert json.loads(path.read_text())["entries"][0]["sessions"][0]["ended_at"]
