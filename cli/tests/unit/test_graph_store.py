"""Unit tests for fno.graph.store - the keeper-backed client.

These tests are ported from tests/test_graph.py and target the extracted module.
They run the module functions directly (no subprocess) for speed.
"""
from __future__ import annotations

import base64
import json
import os
import time
import types
from pathlib import Path

import pytest

from fno.rust_binary import find_dev_binary
from fno.graph.store import (
    GraphCorruptError,
    _apply_graph_defaults,
    append_session_record,
    _read_json,
    commit_rows_via_store,
    read_graph_strict,
    render_canonical_views,
)

# Since the store port every test here rides the keeper, so the module needs
# the compiled runtime and skips whole where the smoke harness deleted the
# worker binary (the parity-test convention).
requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)

pytestmark = requires_rust


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
    """AC7-EDGE: a legacy row the node model cannot represent (no title)
    round-trips through a mutation VERBATIM: the raw carry preserves every
    field the store has no column for, and the mutation still lands."""
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
    commit_rows_via_store(p, mutator)
    saved = read_graph_strict(p)[0]
    assert saved["session_id"] == "worker-7"  # carried verbatim
    assert saved["details"] == "touched"  # the mutation landed on the raw row


def test_the_raw_flock_helpers_are_retired():
    """The unbounded flock was the port's first named defect: acquisition
    now happens only inside the keeper's bounded lock, so the client exposes
    no raw acquire/release pair to call."""
    import fno.graph.store as store_mod

    assert not hasattr(store_mod, "_acquire_flock")
    assert not hasattr(store_mod, "_release_flock")


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
    entries = read_graph_strict(path)
    assert entries[0]["artifact_url"] is None


def test_scenario1_lazy_migration_completion_note_default(tmp_path):
    """Scenario 1 (HP): Legacy entry without completion_note key gets None on read."""
    path = _make_graph(tmp_path, [{"id": "ab-legacy02", "title": "T"}])
    entries = read_graph_strict(path)
    assert entries[0]["completion_note"] is None


def test_scenario3_edge_preserves_shim_artifact_url(tmp_path):
    """Scenario 3 (EDGE): setdefault preserves pre-set shim values."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-shim0001", "title": "T", "artifact_url": "https://figma/foo"}],
    )
    entries = read_graph_strict(path)
    assert entries[0]["artifact_url"] == "https://figma/foo"


def test_scenario3_edge_preserves_shim_completion_note(tmp_path):
    """Scenario 3 (EDGE): setdefault preserves pre-set completion_note."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-shim0002", "title": "T", "completion_note": "closed Q2"}],
    )
    entries = read_graph_strict(path)
    assert entries[0]["completion_note"] == "closed Q2"


def test_ac1_hp_commit_rows_via_store(tmp_path):
    """AC1-HP: commit_rows_via_store reads, applies mutator, writes back."""
    path = tmp_path / "graph.json"

    def mutator(entries):
        entries.append({"id": "ab-newnode0", "title": "New"})
        return entries

    commit_rows_via_store(path, mutator)
    # The store write lands in graph.db; the json file is only an export.
    result = read_graph_strict(path)
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

    commit_rows_via_store(path, mutator)
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

    commit_rows_via_store(path, mutator)
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

    commit_rows_via_store(path, mutator)
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

    commit_rows_via_store(path, mutator)
    result = _read_json(path)
    blocked = next(e for e in result if e["id"] == "ab-blocked")
    assert blocked.get("touched_at") == "2020-01-01T00:00:00+00:00"


def test_render_pass_fail_open_when_vault_root_raises(tmp_path, monkeypatch):
    """A malformed settings file that makes vault_root() raise must not crash
    the view pass (Codex P2 on PR #430): graph.json is already written, so the
    Obsidian-gating decision falls open to no-scaffolding."""
    import fno.paths as paths_mod

    import fno.graph._constants as gc

    def boom():
        raise RuntimeError("malformed settings")

    monkeypatch.setattr(paths_mod, "vault_root", boom)
    path = tmp_path / "graph.json"
    monkeypatch.setattr("fno.paths.graph_json", lambda: path)
    monkeypatch.setitem(vars(gc), "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setitem(vars(gc), "GRAPH_HTML", tmp_path / "graph.html")

    def mutator(entries):
        entries.append({"id": "ab-failopen", "title": "FailOpen"})
        return entries

    # Must not raise despite vault_root() blowing up.
    commit_rows_via_store(path, mutator)
    render_canonical_views()
    # The store write lands in graph.db; the json file is only an export.
    result = read_graph_strict(path)
    assert any(e.get("id") == "ab-failopen" for e in result)
    # graph.md rendered, fail-open without Obsidian frontmatter.
    md = (tmp_path / "graph.md").read_text()
    assert "kanban-plugin: board" not in md


def test_regression_view_pass_renders_the_store_not_global(tmp_path, monkeypatch):
    """Regression: the view pass renders the canonical store it reads, never
    the global ~/.fno targets.

    Guards the board-server bug where running the test suite clobbered the
    real ~/.fno/graph.html (served by serve_board.py over Tailscale) with
    single-fixture-node renders. Simulate the global location via a
    monkeypatched state_dir; if the pass ever falls back to the global
    default again, the fake_home assertions below trip instead of polluting
    the developer's actual ~/.fno.
    """
    import fno.graph._constants as gc

    fake_home = tmp_path / "fake_home_fno"
    fake_home.mkdir()
    monkeypatch.setattr(
        "fno.graph._constants._state_dir", lambda: fake_home
    )

    graph_dir = tmp_path / "work"
    graph_dir.mkdir()
    path = graph_dir / "graph.json"
    monkeypatch.setattr("fno.paths.graph_json", lambda: path)
    monkeypatch.setitem(vars(gc), "GRAPH_HTML", graph_dir / "graph.html")
    monkeypatch.setitem(vars(gc), "GRAPH_MD", graph_dir / "graph.md")

    def mutator(entries):
        entries.append({"id": "ab-sibling1", "title": "Sib"})
        return entries

    commit_rows_via_store(path, mutator)
    render_canonical_views()

    # Renders land beside the canonical store the pass read.
    assert (graph_dir / "graph.html").exists()
    assert (graph_dir / "graph.md").exists()
    # The (simulated) global location is never written.
    assert not (fake_home / "graph.html").exists()


def test_canonical_graph_renders_to_board_targets(tmp_path, monkeypatch):
    """A write to the canonical graph.json renders to GRAPH_HTML/GRAPH_MD
    (what `fno backlog view` and serve_board.py read), not graph.json's
    siblings.

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
    # Pin the RESOLVER, not the facade: canonicality reads paths.graph_json(),
    # and a facade setattr's undo bakes the path into the module (see
    # test_archive_sweep). The render targets are read through the facade, so
    # they patch by setitem to keep the undo unbaking.
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph_json)
    monkeypatch.setitem(vars(gc), "GRAPH_HTML", state_dir / "graph.html")
    monkeypatch.setitem(vars(gc), "GRAPH_MD", state_dir / "graph.md")

    def mutator(entries):
        entries.append({"id": "ab-canon01", "title": "Canon"})
        return entries

    commit_rows_via_store(graph_json, mutator)
    render_canonical_views()

    # Board targets (state_dir) get the render, not graph.json's siblings.
    assert (state_dir / "graph.html").exists()
    assert (state_dir / "graph.md").exists()
    assert not (custom_dir / "graph.html").exists()


def test_canonical_auto_render_keeps_archive_only_rows(tmp_path, monkeypatch):
    """A write cannot clobber the private served board back to live-only."""
    import fno.graph._constants as gc
    from fno.graph.store import _worker_binary

    if _worker_binary() is None:
        pytest.skip("no fno-agents-worker binary; build with `cargo build -p fno-agents`")

    state_dir = tmp_path / "state"
    state_dir.mkdir()
    graph_json = state_dir / "graph.json"
    # The archived row is a stamped resident of the same store, not a sibling
    # advisory file; it must be seeded before the first db open folds the seed.
    graph_json.write_text(
        json.dumps({"entries": [
            {"id": "ab-archive1", "title": "ARCHIVE-AUTO-RENDER-MARKER",
             "status": "done", "project": "fno",
             "archived_at": "2026-08-01T00:00:00Z"},
        ]}),
        encoding="utf-8",
    )
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_json)
    monkeypatch.setattr(gc, "GRAPH_HTML", state_dir / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_MD", state_dir / "graph.md")
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph_json)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)

    commit_rows_via_store(
        graph_json,
        lambda entries: [*entries, {"id": "ab-live0001", "title": "live"}],
    )
    render_canonical_views()

    assert "ARCHIVE-AUTO-RENDER-MARKER" in (state_dir / "graph.html").read_text()


def test_ac1_hp_read_graph_returns_with_defaults(tmp_path):
    """AC1-HP: read_graph applies defaults to entries."""
    path = _make_graph(tmp_path, [{"id": "ab-12341234", "title": "T"}])
    entries = read_graph_strict(path)
    assert len(entries) == 1
    assert entries[0]["priority"] == "p2"


def test_legacy_underscore_status_key_migrates_on_import(tmp_path):
    """A pre-rename row carries `_status`; the import folds it into `status`
    and runs the same vocabulary migration a literal `status` gets."""
    path = _make_graph(
        tmp_path, [{"id": "ab-12341234", "title": "T", "_status": "claimed"}]
    )
    entry = read_graph_strict(path)[0]
    assert "_status" not in entry
    # claimed -> in_progress (STATUS_MIGRATION), the rename the legacy
    # spelling exists to receive.
    assert entry["status"] == "in_progress"


def _ready_plan_entry(tmp_path: Path, node_id: str = "ab-open0001") -> tuple[Path, dict]:
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\n---\n", encoding="utf-8")
    return plan, {
        "id": node_id,
        "slug": node_id,
        "title": node_id,
        "type": "feature",
        "status": "idea",
        "priority": "p2",
        "cwd": str(tmp_path),
        "plan_path": plan.name,
        "sessions": [],
    }


def test_a_malformed_merge_grant_is_refused_before_any_store_work(tmp_path):
    """The spawner's merge posture rides the do row, but a grant that cannot
    name approved/source/recorded_by/recorded_at is a ValueError at the call
    boundary: nothing reaches the keeper, and no row is written."""
    _plan, entry = _ready_plan_entry(tmp_path)
    path = _make_graph(tmp_path, [entry])

    with pytest.raises(ValueError, match="merge_grant.approved must be a boolean"):
        append_session_record(
            path,
            entry["id"],
            phase="do",
            harness="codex",
            session_id="session-open",
            merge_grant={"approved": "yes", "source": "config",
                         "recorded_by": "spawner", "recorded_at": "2026-08-20T00:00:00Z"},
        )
    assert read_graph_strict(path)[0]["sessions"] == []

    with pytest.raises(ValueError, match="unknown keys"):
        append_session_record(
            path,
            entry["id"],
            phase="do",
            harness="codex",
            session_id="session-open",
            merge_grant={"approved": True, "source": "config",
                         "recorded_by": "spawner", "recorded_at": "2026-08-20T00:00:00Z",
                         "extra": 1},
        )

    grant = {"approved": True, "source": "config",
             "recorded_by": "spawner", "recorded_at": "2026-08-20T00:00:00Z"}
    found, added = append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="session-open",
        started_at="2026-08-20T00:00:00Z",
        merge_grant=grant,
    )
    assert (found, added) == (True, True)
    row = read_graph_strict(path)[0]["sessions"][0]
    assert row["merge_grant"] == grant

    # A re-stamp with a DIFFERENT posture must not rewrite the recorded one.
    found, added = append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="session-open",
        merge_grant={"approved": False, "source": "none",
                     "recorded_by": "spawner", "recorded_at": "2026-08-20T01:00:00Z"},
    )
    assert (found, added) == (True, False)
    row = read_graph_strict(path)[0]["sessions"][0]
    assert row["merge_grant"]["approved"] is True


def test_ac1_hp_one_row_per_session_and_phase_whatever_the_harness_spelling(tmp_path):
    """Two writers stamping the same (session_id, phase) - even with different
    harness spellings - leave exactly one row: the second fills open
    timestamps on the first instead of minting a twin."""
    _plan, entry = _ready_plan_entry(tmp_path)
    path = _make_graph(tmp_path, [entry])

    found, added = append_session_record(
        path, entry["id"], phase="do", harness="claude",
        session_id="legacy-1", started_at="2026-09-04T10:00:00Z",
    )
    assert (found, added) == (True, True)
    found, added = append_session_record(
        path, entry["id"], phase="do", harness="unknown",
        session_id="legacy-1", ended_at="2026-09-04T11:00:00Z",
    )
    assert (found, added) == (True, False)
    rows = read_graph_strict(path)[0]["sessions"]
    assert len(rows) == 1
    assert rows[0]["ended_at"] == "2026-09-04T11:00:00Z"


def test_ac1_err_wrong_shape_harness_is_refused_and_writes_nothing(tmp_path):
    """A codex-shaped id stamped `harness: claude` is the phantom-twin defect:
    ValueError naming the shape and the harness, node unchanged. The same id
    under its own harness stamps fine."""
    _plan, entry = _ready_plan_entry(tmp_path)
    path = _make_graph(tmp_path, [entry])
    codex_id = "01a06886-9405-74a1-8afd-5b67baf89604"

    with pytest.raises(
        ValueError, match=r"is a codex id; refusing harness claude"
    ):
        append_session_record(
            path, entry["id"], phase="do", harness="claude", session_id=codex_id,
        )
    assert read_graph_strict(path)[0]["sessions"] == []

    found, added = append_session_record(
        path, entry["id"], phase="do", harness="codex", session_id=codex_id,
    )
    assert (found, added) == (True, True)

    # A v4 id under claude is legal, and a grok thread carrying a minted v4
    # id is never refused on shape.
    found, added = append_session_record(
        path, entry["id"], phase="review", harness="claude",
        session_id="b936b571-e0aa-40ed-a07d-97acb9a87db1",
    )
    assert (found, added) == (True, True)
    found, added = append_session_record(
        path, entry["id"], phase="ship", harness="grok",
        session_id="8ad8e13c-1111-4222-8333-444455556666",
    )
    assert (found, added) == (True, True)


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
    saved = read_graph_strict(path)[0]
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
    saved = read_graph_strict(path)[0]
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
    assert read_graph_strict(path)[0]["status"] == "in_progress"

    append_session_record(
        path,
        entry["id"],
        phase="do",
        harness="codex",
        session_id="session-two",
        ended_at="2026-08-20T00:02:00Z",
    )
    assert read_graph_strict(path)[0]["status"] == "ready"


def test_reap_open_session_record_fills_exact_open_row_with_readback(tmp_path):
    """AC3/AC4: observer reaping fills one exact open row and settles status."""
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
        "row_removed": False,
        # x-7214: every phase, do included, closes by filling ended_at.
        "row_closed": True,
        "status_before": "in_progress",
        "status_after": "in_progress",
        "remaining_open_do": 1,
        # The keeper's receipt names the settled node on every form.
        "node_ids": ["ab-reap0001"],
    }
    rows = read_graph_strict(path)[0]["sessions"]
    assert [(r["harness"], r["session_id"], bool(r.get("ended_at"))) for r in rows] == [
        ("codex", "dead-session", True),
        ("codex", "live-session", False),
    ]


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
    assert read_graph_strict(path)[0]["sessions"][0]["ended_at"]


# -- blocked_by edge settlement (settle_edges verb) --


def test_settle_blocked_by_edges_prunes_rewires_and_holds():
    """The full sweep's write-side twin of the readiness chase: an edge to a
    done blocker prunes, one superseded by an open successor rewires to name
    it, and a deferred or missing blocker holds with a receipt naming why."""
    from fno.graph.store import settle_blocked_by_edges_via_store

    entries = [
        {"id": "ab-1", "blocked_by": ["ab-done"]},
        {"id": "ab-done", "completed_at": "2026-09-01T00:00:00Z"},
        {"id": "ab-2", "blocked_by": ["ab-old"]},
        {"id": "ab-old", "superseded_by": "ab-new"},
        {"id": "ab-new"},
        {"id": "ab-3", "blocked_by": ["ab-def"]},
        {"id": "ab-def", "deferred_at": "2026-08-01T00:00:00Z"},
        {"id": "ab-4", "blocked_by": ["ab-ghost"]},
        {"id": "ab-5", "blocked_by": ["ab-live"]},
        {"id": "ab-live"},
    ]
    out = settle_blocked_by_edges_via_store(entries)
    by_id = {e["id"]: e for e in out["entries"]}
    assert by_id["ab-1"]["blocked_by"] == []
    assert by_id["ab-2"]["blocked_by"] == ["ab-new"]
    # Deferred and missing hold: a human decision and data loss are not a
    # sweep's to erase.
    assert by_id["ab-3"]["blocked_by"] == ["ab-def"]
    assert by_id["ab-4"]["blocked_by"] == ["ab-ghost"]
    # A live blocker gets no receipt: a correct edge is not a finding.
    assert by_id["ab-5"]["blocked_by"] == ["ab-live"]
    kinds = sorted(r["kind"] for r in out["receipts"])
    assert kinds == [
        "blocked_by_held",
        "blocked_by_held",
        "blocked_by_pruned",
        "blocked_by_rewired",
    ]
    rewired = next(r for r in out["receipts"] if r["kind"] == "blocked_by_rewired")
    assert rewired["node"] == "ab-2"
    assert rewired["blocker"] == "ab-old"
    assert set(out["blocked_by"].keys()) == {"ab-1", "ab-2"}


def test_settle_blocked_by_edges_superseded_by_done_prunes_with_the_chain():
    """A dead blocker whose successor already shipped prunes, and the receipt
    names the chain so the receipt alone explains the drop."""
    from fno.graph.store import settle_blocked_by_edges_via_store

    entries = [
        {"id": "ab-1", "blocked_by": ["ab-old"]},
        {"id": "ab-old", "superseded_by": "ab-done"},
        {"id": "ab-done", "completed_at": "2026-09-01T00:00:00Z"},
    ]
    out = settle_blocked_by_edges_via_store(entries)
    by_id = {e["id"]: e for e in out["entries"]}
    assert by_id["ab-1"]["blocked_by"] == []
    (receipt,) = out["receipts"]
    assert receipt["kind"] == "blocked_by_pruned"
    assert "superseded by ab-done" in receipt["reason"]


def test_sweep_kills_only_the_keeper_whose_graph_is_gone(tmp_path):
    """Positive control for the orphan sweep (the keeper-leak class of
    2026-09-04).

    The sweep's kill decision is the ``--graph`` path's existence, never the
    command line: the canonical keepers and the leaked ones share a command
    line, and an argv match killed the two processes serving the real graph
    on 2026-09-04. Two real sleepers advertise identical keeper argv; only
    the one whose graph directory is deleted may die. Both must be visible
    to the SAME probe the sweep uses before it acts - a zero-after read is
    meaningless unless the probe first named its target.
    """
    import shutil
    import subprocess
    import sys

    from fno.graph import store as store_mod

    def _advertised_keeper(graph: Path) -> subprocess.Popen:
        # A real child whose argv carries the keeper's flags. The sleeper
        # body ignores them; the ps scan must not.
        return subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)",
             "--store-keeper", "--sock", f"{graph}.sock",
             "--graph", str(graph), "--session", "sweep-test"],
        )

    doomed_dir = tmp_path / "doomed"
    doomed_dir.mkdir()
    doomed_graph = doomed_dir / "graph.json"
    doomed_graph.write_text('{"entries": []}\n')
    doomed = _advertised_keeper(doomed_graph)
    kept_graph = tmp_path / "kept" / "graph.json"
    kept_graph.parent.mkdir()
    kept_graph.write_text('{"entries": []}\n')
    kept = _advertised_keeper(kept_graph)
    try:
        assert doomed.poll() is None and kept.poll() is None
        # Pre-kill census through the sweep's own probe, by pid not by
        # count: with both graphs present neither is a candidate (the
        # discriminator), and after the rmtree the doomed one must be a
        # NAMED candidate - the non-zero control for the sweep's kill.
        assert doomed.pid not in store_mod._orphaned_keeper_pids()
        shutil.rmtree(doomed_dir)
        candidates = store_mod._orphaned_keeper_pids()
        assert doomed.pid in candidates, (
            f"the graph-gone sleeper must be a named candidate before the "
            f"sweep; the probe saw {candidates}"
        )
        assert kept.pid not in candidates, (
            "the graph-alive sleeper must survive the probe: identical argv, "
            "the graph's existence decides"
        )

        assert store_mod.sweep_orphaned_keepers(timeout=15.0) == []
        doomed.wait(timeout=15)
        assert doomed.poll() is not None, "the graph-gone keeper must die"
        assert kept.poll() is None, "the graph-alive keeper must survive"
    finally:
        for proc in (doomed, kept):
            proc.kill()
            proc.wait(timeout=15)


# -- the by-id read --


def test_read_nodes_by_ids_returns_exact_rows(tmp_path):
    """AC9-HP client half: one id, one row back, nothing unmatched."""
    from fno.graph.store import read_nodes_by_ids

    path = _make_graph(
        tmp_path,
        [
            {"id": "ab-1", "slug": "first-one", "title": "One"},
            {"id": "ab-2", "slug": "second-one", "title": "Two"},
        ],
    )
    result = read_nodes_by_ids(path, ["second-one", "ab-1", "zz-none"])
    assert result is not None
    assert [e["id"] for e in result["entries"]] == ["ab-2", "ab-1"]
    assert result["missing"] == ["zz-none"]


def test_read_nodes_by_ids_returns_none_when_the_keeper_predates_the_verb(tmp_path, monkeypatch):
    """AC10-EDGE: a stale keeper (installed worker behind the source) answers
    `unknown store method`; the fast path degrades to None and the caller
    falls back, so an old binary never breaks a current client."""
    from fno.graph import store as store_mod

    def stale_request(self, method, params):
        raise RuntimeError("store error (invalid): unknown store method \"read_ids\"")

    monkeypatch.setattr(store_mod._Keeper, "request", stale_request)
    monkeypatch.setattr(store_mod._ExecClient, "request", stale_request)
    path = _make_graph(tmp_path, [{"id": "ab-1", "title": "One"}])
    assert store_mod.read_nodes_by_ids(path, ["ab-1"]) is None


def test_run_op_derives_the_rung_map_from_the_light_plan_refs_read(tmp_path, monkeypatch):
    """x-8a09 site one: the typed op derives its plan-rung map from the light
    plan_refs read. A call that needs one derived map must not pay the
    most expensive read in the system (a full begin) for it."""
    from fno.graph import store as store_mod

    plan = tmp_path / "p.md"
    plan.write_text("---\nstatus: design\n---\n# plan\n")
    methods: list[str] = []
    seen: dict = {}

    def fake_request(self, method, params):
        methods.append(method)
        if method == "plan_refs":
            return {"entries": [
                {"id": "ab-1"},
                {"id": "ab-2", "plan_path": str(plan), "cwd": str(tmp_path)},
            ]}
        if method == "op":
            seen.update(params["params"]["plan_rungs"])
            return {"outcome": {"version": "v2"}, "op": {"found": True, "plan_path": "p.md"}}
        raise AssertionError(f"unexpected keeper method {method}")

    monkeypatch.setattr(store_mod._Keeper, "request", fake_request)
    monkeypatch.setattr(store_mod._ExecClient, "request", fake_request)
    monkeypatch.setattr(store_mod, "_finish_mutation", lambda path, outcome: None)
    result = store_mod._run_op(
        tmp_path / "graph.json", "append_progress_note",
        {"node_id": "ab-1", "note": {"ts": "t", "text": "x"}},
    )
    assert result == {"found": True, "plan_path": "p.md"}
    assert methods == ["plan_refs", "op"], "a full begin never fires"
    assert seen == {"ab-1": "none", "ab-2": "design"}


def test_resolve_node_id_serves_the_exact_hit_from_the_by_id_read(tmp_path):
    """Change 4's resolve site: exact id and exact slug through one row,
    no whole-graph begin."""
    from fno.graph import store as store_mod

    path = _make_graph(
        tmp_path,
        [
            {"id": "ab-1", "slug": "first-one", "title": "One"},
            {"id": "ab-2", "slug": "second-one", "title": "Two"},
        ],
    )
    assert store_mod._resolve_node_id(path, "second-one") == "ab-2"
    assert store_mod._resolve_node_id(path, "ab-1") == "ab-1"


def test_resolve_node_id_falls_back_to_the_begin_snapshot(tmp_path, monkeypatch):
    """AC10-EDGE at the resolve site: any fast-path absence keeps riding the
    begin snapshot, so the snapshot resolver's own tiers are unchanged."""
    from fno.graph import store as store_mod

    path = _make_graph(
        tmp_path,
        [{"id": "ab-12345678", "slug": "first-one", "title": "One"}],
    )

    def no_fast(path, tokens):
        return None

    monkeypatch.setattr(store_mod, "read_nodes_by_ids", no_fast)
    # The exact id resolves through the snapshot when the fast path is out.
    assert store_mod._resolve_node_id(path, "ab-12345678") == "ab-12345678"
    # ...and a genuinely absent node still resolves to None.
    assert store_mod._resolve_node_id(path, "zz-none") is None


def test_single_id_get_serves_the_exact_hit_from_the_by_id_read(tmp_path, monkeypatch, capsys):
    """The get fast path: the row renders through the same renderer, the miss
    falls back by returning the token unchanged."""
    import typer

    from fno.graph import get_batch

    row = {"id": "ab-1", "slug": "first-one", "title": "One", "status": "idea"}
    payload = {"entries": [dict(row)], "missing": []}

    def fake_fast(path, tokens):
        return dict(payload)

    # get_batch imports the helper from store at call time; patch it there.
    from fno.graph import store as store_mod

    monkeypatch.setattr(store_mod, "read_nodes_by_ids", fake_fast)
    monkeypatch.setattr(get_batch, "_graph_path", lambda: tmp_path / "graph.json")
    # Exact id: served, rendered, never returns.
    with pytest.raises(typer.Exit) as exc:
        get_batch.resolve_or_dispatch(["ab-1"], field=None, grouped=False, strict=False)
    assert exc.value.exit_code == 0
    assert json.loads(capsys.readouterr().out)["id"] == "ab-1"

    # A case-different id must NOT serve: resolve_node tier 1 is exact, so
    # a fast path hit here would widen resolution.
    payload["entries"] = [dict(row)]
    payload["missing"] = []
    returned = get_batch.resolve_or_dispatch(["AB-1"], field=None, grouped=False, strict=False)
    assert returned == "AB-1"

    # Miss: the token falls through to the caller's full path.
    payload["entries"] = []
    payload["missing"] = ["zz-none"]
    returned = get_batch.resolve_or_dispatch(["zz-none"], field=None, grouped=False, strict=False)
    assert returned == "zz-none"


# -- the bounded retry --

from fno.graph import store as store_mod  # noqa: E402 - the tx-loop section


class _ScriptedClient:
    """A keeper client whose commits conflict a scripted number of times.

    The tx loop's mechanics (backoff, jitter, budget) are client-side, so the
    contention tests script the transport instead of racing real writers."""

    def __init__(self, conflicts: int, path: Path = Path("/tmp/x1601-tx.json")):
        self.conflicts = conflicts
        self.begins = 0
        self.path = path

    def request(self, method, params):
        if method == "begin":
            self.begins += 1
            return {
                "version": f"v{self.begins}",
                "entries": [],
                "base_digests": {},
            }
        if method == "commit_rows":
            if self.conflicts > 0:
                self.conflicts -= 1
                raise store_mod._Conflict()
            return {
                "entries": [],
                "dropped": 0,
                "backup": None,
                "closure_releases": [],
                "is_canonical": True,
            }
        raise AssertionError(f"unexpected method {method}")


def _run_tx(client, monkeypatch, record):
    monkeypatch.setattr(store_mod, "_client_for", lambda _path: client)
    # The post-publish render resolves paths.graph_json() and drives a REAL
    # client (api.py binds _client_for at import, so the patch above does not
    # reach it); with a per-test state root that is a keeper spawn whose
    # poll sleeps land in `record` and read as retry delays. Not under test.
    monkeypatch.setattr(store_mod, "render_view_projections", lambda *a, **k: None)
    # Patch the store's OWN time binding, never the shared time module: the
    # module object is global, so a background drain thread sleeping inside
    # this window would land in `record` too and read as a retry delay.
    monkeypatch.setattr(
        store_mod,
        "time",
        types.SimpleNamespace(
            sleep=record,
            monotonic=store_mod.time.monotonic,
        ),
    )
    return store_mod.commit_rows_via_store(client.path, lambda e: e)


def test_two_colliding_writers_both_land_and_their_delays_differ(tmp_path, monkeypatch):
    """AC13-HP + AC15-HP: the loser retries once and lands (both commits
    land), and the two writers' drawn delays differ - asserted on the drawn
    values through the injected sleep, never wall-clock timing."""
    winner = _ScriptedClient(conflicts=0)
    loser = _ScriptedClient(conflicts=1)
    delays: list[float] = []
    _run_tx(winner, monkeypatch, delays.append)
    _run_tx(loser, monkeypatch, delays.append)
    assert len(delays) == 1, "the un-contended winner must never sleep"
    assert 0.0 <= delays[0] <= store_mod._TX_BACKOFF_BASE_S
    # The draw is real (not injected), so two colliders at the same instant
    # draw different values - the whole point of full jitter.
    second = _ScriptedClient(conflicts=1)
    delays2: list[float] = []
    _run_tx(second, monkeypatch, delays2.append)
    assert delays[0] != delays2[0], "two colliding writers must not draw equal delays"


def test_the_retry_budget_is_bounded_and_every_delay_sits_in_its_band(tmp_path, monkeypatch):
    """AC16-EDGE: across a full five-attempt budget every delay lies within
    its attempt's full-jitter bound and the total wait stays under the stated
    ceiling. A retry budget with no ceiling is the defect in a slower coat."""
    spender = _ScriptedClient(conflicts=4)
    delays: list[float] = []
    _run_tx(spender, monkeypatch, delays.append)
    assert len(delays) == 4, "four conflicts, four sleeps, no sleep after the last"
    total = 0.0
    for attempt, delay in enumerate(delays):
        bound = min(
            store_mod._TX_BACKOFF_BASE_S * 2**attempt,
            store_mod._TX_BACKOFF_CAP_S,
        )
        assert 0.0 <= delay <= bound, f"attempt {attempt} drew {delay}, bound {bound}"
        total += delay
    # The stated ceiling: every band summed, since the bands are the whole
    # budget the loop can spend before the fifth attempt raises.
    ceiling = sum(
        min(store_mod._TX_BACKOFF_BASE_S * 2**attempt, store_mod._TX_BACKOFF_CAP_S)
        for attempt in range(4)
    )
    assert total <= ceiling + 1e-9


def test_the_spent_budget_raises_the_existing_error_unchanged(tmp_path, monkeypatch):
    """AC14-EDGE: the failure contract is not part of this change - same
    RuntimeError type, same message, when every attempt conflicts."""
    doomed = _ScriptedClient(conflicts=store_mod._TX_ATTEMPTS)
    delays: list[float] = []
    with pytest.raises(RuntimeError) as exc:
        _run_tx(doomed, monkeypatch, delays.append)
    assert str(exc.value) == (
        f"graph mutated under us {store_mod._TX_ATTEMPTS} times at "
        "/tmp/x1601-tx.json; nothing was written, retry when the fleet quiets"
    )
    assert len(delays) == store_mod._TX_ATTEMPTS - 1, (
        "the final conflict raises without a trailing sleep"
    )

def test_dead_socket_serves_by_exec_and_never_spawns(tmp_path, monkeypatch):
    """The spawn-needed branch execs a one-shot lane: no resident keeper is
    minted, so nothing holds the store to grow on. A live incumbent is still
    preferred, and the exec client answers typed helpers."""
    graph = tmp_path / "graph.json"
    graph.write_text('{"entries": []}')
    monkeypatch.setattr(
        store_mod,
        "_worker_binary",
        lambda: tmp_path / "absent-worker",
    )
    client = store_mod._client_for(graph)
    assert isinstance(client, store_mod._ExecClient)
    assert not hasattr(client, "sock"), "the exec transport opens no socket"

    # A live incumbent still rides the socket: the connect probe answers, so
    # _client_for returns the _Keeper without consulting the exec route.
    class _FakeStream:
        def close(self):
            pass

    monkeypatch.setattr(
        store_mod._Keeper,
        "_connect",
        lambda self: _FakeStream(),
    )
    keeper = store_mod._client_for(graph)
    assert isinstance(keeper, store_mod._Keeper)
    assert keeper.sock == store_mod.store_socket_for(graph)


_CHUNK_CAP = 8192


class _CappedStream:
    """Fake stream handing out at most 8192 bytes per call, through both recv
    and recv_into, so the chunk size is identical on every OS (a real
    socketpair on Linux hands out large chunks and would pass the old loop)."""

    def __init__(self, data: bytes):
        self.data = data
        self.pos = 0

    def _next_chunk(self) -> bytes:
        chunk = self.data[self.pos : self.pos + _CHUNK_CAP]
        self.pos += len(chunk)
        return chunk

    def recv(self, n: int) -> bytes:
        return self._next_chunk()

    def recv_into(self, view, n=None) -> int:
        chunk = self._next_chunk()
        view[: len(chunk)] = chunk
        return len(chunk)


def test_recv_exact_linear_on_16mb_frame():
    """AC1-HP: a 16,044,243-byte reply (the measured graph frame) read
    in <=8192-byte chunks returns equal bytes in under 2s. The old
    `data += chunk` loop on bytes copies the whole buffer per chunk and measured
    ~6s of pure copy on this payload."""
    from fno.graph.store import _recv_exact

    payload = (b"a" + bytes(range(1, 256)) * 64) * (16_044_243 // (1 + 255 * 64) + 1)
    payload = payload[:16_044_243]
    stream = _CappedStream(payload)
    start = time.monotonic()
    out = _recv_exact(stream, len(payload))
    elapsed = time.monotonic() - start
    assert out == payload
    assert elapsed < 2.0


def test_recv_exact_refuses_silent_close_midframe():
    """AC1-ERR: a stream that goes silent after half the payload
    raises StoreUnavailable (state silent, keeper-closed wording), never a
    short frame."""
    from fno.graph.store import STATE_SILENT, StoreUnavailable, _recv_exact

    class _DyingStream:
        def __init__(self, serve: int):
            self.serve = serve

        def recv_into(self, view, n=None) -> int:
            if self.serve <= 0:
                return 0
            take = min(_CHUNK_CAP, len(view), self.serve)
            view[:take] = b"\x00" * take
            self.serve -= take
            return take

    stream = _DyingStream(serve=16_044_243 // 2)
    with pytest.raises(StoreUnavailable) as exc:
        _recv_exact(stream, 16_044_243)
    assert exc.value.state == STATE_SILENT
    assert exc.value.detail == "keeper closed the connection mid-frame"


def test_sent_write_resolves_done_and_restores_elided_entries(monkeypatch):
    from fno.graph import store as store_mod

    client = store_mod._Keeper(Path("/tmp/graph.store.sock"))
    replies = iter([
        {
            "state": "done",
            "reply": {
                "ok": True,
                "result": {"entries": None, "entries_elided": True},
            },
        },
        {
            "bytes_b64": base64.b64encode(
                json.dumps({"entries": [{"id": "x-written", "title": "written"}]}).encode()
            ).decode(),
        },
    ])
    monkeypatch.setattr(store_mod._Keeper, "request", lambda self, method, params: next(replies))

    result = client._resolve_write(
        "r1", "commit_rows", store_mod.StoreUnavailable(store_mod.STATE_SILENT, "timed out")
    )

    assert result["entries"] == [{"id": "x-written", "title": "written"}]


def test_sent_write_against_stale_keeper_is_unconfirmed(monkeypatch):
    from fno.graph import store as store_mod

    client = store_mod._Keeper(Path("/tmp/graph.store.sock"))
    monkeypatch.setattr(
        store_mod._Keeper,
        "request",
        lambda self, method, params: (_ for _ in ()).throw(
            RuntimeError('store error (invalid): unknown store method "write_status"')
        ),
    )

    with pytest.raises(store_mod.WriteUnconfirmed) as exc:
        client._resolve_write(
            "r1", "commit_rows", store_mod.StoreUnavailable(store_mod.STATE_SILENT, "timed out")
        )
    assert exc.value.state == store_mod.STATE_UNCONFIRMED
    assert "commit_rows was sent" in str(exc.value)


def test_unconfirmed_commit_names_changed_ids_before_retrying(tmp_path, monkeypatch):
    from fno.graph import store as store_mod

    class UnconfirmedClient:
        path = tmp_path / "graph.json"

        def request(self, method, params):
            if method == "begin":
                return {
                    "version": "v1",
                    "entries": [{"id": "x-seed", "title": "seed", "type": "feature",
                                 "status": "idea", "priority": "p2"}],
                    "base_digests": {"x-seed": "d1"},
                }
            if method == "commit_rows":
                raise store_mod.WriteUnconfirmed(store_mod.STATE_UNCONFIRMED, "outcome unknown")
            raise AssertionError(f"unexpected method {method}")

    monkeypatch.setattr(store_mod, "_client_for", lambda _path: UnconfirmedClient())
    with pytest.raises(store_mod.WriteUnconfirmed) as exc:
        store_mod.commit_rows_via_store(
            UnconfirmedClient.path,
            lambda entries: entries + [{"id": "x-minted", "title": "minted", "type": "feature",
                                        "status": "idea", "priority": "p2"}],
        )
    assert exc.value.ids == ["x-minted"]
    assert "x-minted" in str(exc.value)
    assert "fno backlog get" in str(exc.value)


def test_write_connect_failure_says_write_was_not_sent(monkeypatch):
    from fno.graph import store as store_mod

    client = store_mod._Keeper(Path("/tmp/graph.store.sock"))

    def no_listener():
        raise store_mod.StoreUnavailable(store_mod.STATE_NO_LISTENER, "nothing is listening")

    monkeypatch.setattr(client, "_connect", no_listener)
    with pytest.raises(store_mod.StoreUnavailable) as exc:
        client.request("commit_rows", {})
    assert not isinstance(exc.value, store_mod.WriteUnconfirmed)
    assert "the write was not sent" in str(exc.value)
