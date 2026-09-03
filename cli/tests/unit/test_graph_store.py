"""Unit tests for fno.graph.store - the keeper-backed client.

These tests are ported from tests/test_graph.py and target the extracted module.
They run the module functions directly (no subprocess) for speed.
"""
from __future__ import annotations

import json
import os
import time
from pathlib import Path

import pytest

from fno.rust_binary import find_dev_binary
from fno.graph.store import (
    GraphCorruptError,
    _apply_graph_defaults,
    append_session_record,
    _read_json,
    _write_json,
    locked_mutate_graph,
    read_graph,
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


def test_the_reaper_reaps_a_keeper_it_can_name(tmp_path, monkeypatch):
    """Positive control for the spawn ledger (the zero-filter rule).

    A filter that reports zero proves nothing unless it can first name a
    target it DID see. This test makes the ledger name a real keeper pid,
    asserts that pid is alive before the reap, and asserts the same pid is
    dead after it - by number, never by absence.
    """
    from fno.graph import store as store_mod

    # Clear the field first: earlier tests in this worker process spawned
    # keepers too, and the session's 5s idle bound (conftest) may already
    # have reaped them. The control then names ITS OWN spawn - a dead pid
    # left in the ledger is the clock's prior work, not a reaper miss.
    assert store_mod.reap_spawned_keepers(timeout=15.0) == []
    # Idle exit disabled for this control's keeper: the reaper, not the
    # clock, must be what kills it.
    monkeypatch.setenv("FNO_STORE_KEEPER_IDLE_SECS", "0")

    graph = _make_graph(tmp_path, [{"id": "ab-reap", "title": "reap me"}])
    _client = store_mod._client_for(graph)  # spawns the keeper on demand
    keeper = _client.request("read", {"strict": False, "keep_malformed": False})
    assert keeper["entries"], "keeper must answer before the reap control"

    ledger = store_mod._SPAWNED_KEEPERS
    assert ledger, "a spawned keeper must be addressable in the spawn ledger"
    pid = next(iter(ledger))
    proc, _sock = ledger[pid]
    assert proc.poll() is None, "the named keeper must be alive before the reap"

    survivors = store_mod.reap_spawned_keepers(timeout=15.0)
    assert survivors == [], f"reaper left {len(survivors)} keeper(s) alive"
    with pytest.raises(ProcessLookupError):
        os.kill(pid, 0)
    assert proc.poll() is not None, "the named keeper must be dead after the reap"


# Pids the fixture-under-test pair leaves behind for its mid-run leg. Module
# global because the assertion that matters spans a fixture boundary: xdist
# workers are long-lived, so the pid list survives from one test to the next
# inside the same worker process.
_ZOMBIE_PROBE_PIDS: list[int] = []


def _zombie_children_of_this_process() -> set[int]:
    """Zombie pids among this process's direct children, read from psutil.

    psutil, not a shell pipeline: a shell `ps | wc -l` gave three different
    answers in four minutes on the machine that measured this defect,
    including 31 when the truth was 1800.
    """
    import psutil

    me = psutil.Process()
    return {
        child.pid
        for child in me.children(recursive=False)
        if child.status() == psutil.STATUS_ZOMBIE
    }


def _wait_until_zombies(pids: list[int], timeout: float = 15.0) -> set[int]:
    """Wait (polling ps, never the Popen handles) until every pid is a zombie.

    poll() would reap the child and erase the evidence, so the Popen handles
    must stay untouched until the drain under test. Keepers read
    FNO_STORE_KEEPER_IDLE_SECS at spawn, so the caller pins it short first.
    """
    deadline = time.monotonic() + timeout
    zombies: set[int] = set()
    wanted = set(pids)
    while time.monotonic() < deadline:
        zombies = _zombie_children_of_this_process() & wanted
        if zombies == wanted:
            return zombies
        time.sleep(0.25)
    return zombies


def _spawn_idle_keepers(tmp_path: Path, count: int) -> list[int]:
    from fno.graph import store as store_mod

    pids = []
    for i in range(count):
        graph = tmp_path / f"drain-{i}.json"
        graph.write_text('{"entries": []}\n')
        pids.append(store_mod._spawn_keeper(graph).pid)
    return pids


def _assert_reaped_or_reused(pids: list[int]) -> None:
    """Every pid must have LEFT the zombie state: collected out of the table,
    or its pid number already reused by a live process on a fast-cycling host.
    Either proves the drain reaped it - a pid still answering zombie IS the
    unreaped child. (A bare NoSuchProcess assert would false-fail on reuse.)"""
    import psutil

    for pid in pids:
        try:
            status = psutil.Process(pid).status()
        except psutil.NoSuchProcess:
            continue
        assert status != psutil.STATUS_ZOMBIE, (
            f"pid {pid} still answers zombie; the drain did not reap it"
        )


def test_exited_keepers_are_zombies_until_drained(tmp_path, monkeypatch):
    """Positive control for the mid-run zombie defect and its drain.

    A keeper that self-exits stays in the process table as a zombie under
    this worker's pid until someone collects its status; before the drain
    existed, the only collector was the session-scoped teardown, so a run
    accumulated ~52 zombies per minute under four xdist workers. Measured by
    number, mid-run: three keepers must appear as zombies BEFORE any drain,
    and drain_exited_keepers() must reap exactly those three.
    """
    from fno.graph import store as store_mod

    monkeypatch.setenv("FNO_STORE_KEEPER_IDLE_SECS", "1")
    pids = _spawn_idle_keepers(tmp_path, 3)
    zombies = _wait_until_zombies(pids)
    assert zombies == set(pids), (
        f"exited keepers must appear as zombies under this pid before any "
        f"drain; saw {sorted(zombies)} of {sorted(pids)}"
    )

    reaped = store_mod.drain_exited_keepers()
    assert reaped == len(pids), f"drain must name every exited keeper; reaped {reaped}"
    _assert_reaped_or_reused(pids)


@pytest.mark.xdist_group(name="keeper-zombie")
def test_spawned_zombies_persist_into_next_test_without_drain(tmp_path, monkeypatch):
    """Spawn leg of the mid-run pair: leave exited keepers behind, drain none.

    The body deliberately does NOT reap; the autouse drain fixture under test
    is what should collect these before the companion assertion runs. On code
    without the fixture the companion fails naming these pids, which is the
    defect reproduced live.
    """
    monkeypatch.setenv("FNO_STORE_KEEPER_IDLE_SECS", "1")
    pids = _spawn_idle_keepers(tmp_path, 3)
    zombies = _wait_until_zombies(pids)
    assert zombies == set(pids), (
        f"keepers must be exited zombies before this test ends; saw "
        f"{sorted(zombies)} of {sorted(pids)}"
    )
    _ZOMBIE_PROBE_PIDS.clear()
    _ZOMBIE_PROBE_PIDS.extend(pids)


@pytest.mark.xdist_group(name="keeper-zombie")
def test_drain_fixture_reaps_zombies_between_tests_midrun():
    """Mid-run leg of the pair: the previous test's zombies must be gone.

    The session-scoped reaper has NOT run yet - this assertion fires between
    tests, exactly where the defect lived. No recorded pid may still answer
    zombie (reaped, not merely SIGTERMed). Pair runs on one xdist worker via
    the keeper-zombie group; alone it would assert nothing, so it skips.
    """
    if not _ZOMBIE_PROBE_PIDS:
        pytest.skip("companion spawn leg did not run in this worker")
    still_zombie = [
        pid
        for pid in _ZOMBIE_PROBE_PIDS
        if pid in _zombie_children_of_this_process()
    ]
    assert not still_zombie, (
        f"{len(still_zombie)} keeper zombie(s) survived past the test "
        f"boundary mid-run (pids {still_zombie}); the autouse drain fixture "
        f"must reap between tests, not only at session teardown"
    )
    _assert_reaped_or_reused(_ZOMBIE_PROBE_PIDS)


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
    assert json.loads(path.read_text())["entries"][0]["sessions"] == []

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
    row = json.loads(path.read_text())["entries"][0]["sessions"][0]
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
    row = json.loads(path.read_text())["entries"][0]["sessions"][0]
    assert row["merge_grant"]["approved"] is True


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
