"""Unit tests for parallel-mode lane dispatch (x-8b48, group 3).

Covers `advance.dispatch_lanes` and its isolation helpers: one isolated worktree
+ bg worker per selected lane, per-lane `.fno/config.local.toml` seeding with a
DISTINCT project.id (x-071c narrowed the seed to this sole key), and
slot-release-on-failure so one lane's spawn failure never aborts the fleet
(Failure Modes: Errors).

`select_lane_fill` runs for real against a monkeypatched `_ready_nodes` and an
isolated `tmp_path` claims root, so the lane slots are genuinely held and the
release path is exercised. `_ensure_lane_worktree` / `_spawn_worker` are
monkeypatched (no real git / spawn).
"""
from __future__ import annotations

from pathlib import Path

import tomllib

from fno.backlog import advance
from fno.claims.lanes import active_lane_count, find_lane_slot
from fno.config import WORKTREE_LOCAL_KEYS, _worktree_local_override


def _nodes(*specs):
    return [{"id": i, "domain": d, "title": i, "slug": i} for i, d in specs]


def _wire(monkeypatch, tmp_path, ready, *, spawn=None):
    """Mock the dispatch seams. Returns a dict recording calls."""
    # The dispatch:<id> boot-window reservation is GLOBAL-rooted; pin the global
    # claims root into tmp so it lands in the same isolated dir as the explicit
    # lane-slot root (claims_root=tmp_path/"claims") and never touches ~/.fno.
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))
    monkeypatch.setattr(advance, "_canonical_root", lambda: tmp_path / "canonical")
    monkeypatch.setattr(advance, "_base_project_id", lambda root: "fno")

    calls: dict = {"worktrees": [], "spawns": []}

    def fake_ensure(node_id, *, canonical_root, harness="claude"):
        wt = tmp_path / "wt" / node_id
        (wt / ".fno").mkdir(parents=True, exist_ok=True)
        calls["worktrees"].append((node_id, harness))
        return wt

    def fake_spawn(node_id, cwd, slug, model=None, provider=None, **kwargs):
        calls["spawns"].append((node_id, cwd, slug))
        if spawn is not None:
            return spawn(node_id)
        return f"short-{node_id}"

    monkeypatch.setattr(advance, "_ensure_lane_worktree", fake_ensure)
    monkeypatch.setattr(advance, "_spawn_worker", fake_spawn)
    return calls


def test_dispatch_spawns_one_isolated_worker_per_distinct_domain_lane(
    tmp_path, monkeypatch
):
    ready = _nodes(("n-a", "code"), ("n-b", "code"), ("n-c", "docs"))
    calls = _wire(monkeypatch, tmp_path, ready)

    receipts = advance.dispatch_lanes(
        3, project_root=tmp_path, claims_root=tmp_path / "claims"
    )

    # Distinct-domain selection -> n-a (code) + n-c (docs); n-b (dup code) queued.
    assert [r["node_id"] for r in receipts] == ["n-a", "n-c"]
    assert all(r["status"] == "dispatched" for r in receipts)
    # (node_id, harness): no node carries a provider, so the lane worktree is
    # isolated with the claude default harness (harness-native placement).
    assert calls["worktrees"] == [("n-a", "claude"), ("n-c", "claude")]
    # Each worker is rooted in its OWN lane worktree (--cwd = the ensured path).
    for node_id, cwd, _slug in calls["spawns"]:
        assert cwd == str(tmp_path / "wt" / node_id)
    # Slots stay held: the worker reconciles them at target init (LD#8).
    assert active_lane_count(root=tmp_path / "claims") == 2


def test_seed_writes_only_allowlist_keys_distinct_per_lane(tmp_path):
    """AC7-EDGE: the seed emits project.id ONLY (x-071c dropped parking_lot_path);
    two lanes get distinct per-lane ids."""
    wt_a = tmp_path / "a"
    wt_b = tmp_path / "b"
    advance._seed_lane_local_settings(wt_a, "n-a", "fno")
    advance._seed_lane_local_settings(wt_b, "n-b", "fno")

    raw_a = tomllib.loads((wt_a / ".fno" / "config.local.toml").read_text())
    raw_b = tomllib.loads((wt_b / ".fno" / "config.local.toml").read_text())

    # The seed must contain NO parking_lot_path (x-071c: the post-merge ritual
    # anchors on the canonical root, so a per-lane redirect is gone).
    assert "post_merge" not in raw_a
    assert "post_merge" not in raw_b

    # x-cbce loader keeps ONLY the allowlist; a seeded key outside it would be
    # dropped here, so passing through the loader proves the seed is well-formed.
    ov_a = _worktree_local_override(raw_a)
    ov_b = _worktree_local_override(raw_b)
    assert set(_leaf_paths(ov_a)) == set(WORKTREE_LOCAL_KEYS)

    # project.id is per-lane (neuters nested auto-continue).
    assert ov_a["project"]["id"] == "fno-n-a"
    assert ov_b["project"]["id"] == "fno-n-b"


def _leaf_paths(d, prefix=""):
    """Dotted leaf paths of a nested override dict (for allowlist assertions)."""
    for k, v in d.items():
        path = f"{prefix}{k}"
        if isinstance(v, dict):
            yield from _leaf_paths(v, f"{path}.")
        else:
            yield path


def test_spawn_failure_releases_slot_and_spares_other_lanes(tmp_path, monkeypatch):
    ready = _nodes(("n-a", "code"), ("n-c", "docs"))

    def spawn(node_id):
        if node_id == "n-a":
            raise advance.SpawnError("boom")
        return f"short-{node_id}"

    _wire(monkeypatch, tmp_path, ready, spawn=spawn)

    receipts = advance.dispatch_lanes(
        3, project_root=tmp_path, claims_root=tmp_path / "claims"
    )

    by_id = {r["node_id"]: r for r in receipts}
    assert by_id["n-a"]["status"] == "skipped"
    assert "boom" in by_id["n-a"]["error"]
    assert by_id["n-c"]["status"] == "dispatched"
    # The failed lane released its slot -> re-dispatchable; the good lane keeps its.
    assert find_lane_slot("n-a", root=tmp_path / "claims") is None
    assert find_lane_slot("n-c", root=tmp_path / "claims") is not None
    assert active_lane_count(root=tmp_path / "claims") == 1


def test_max_lanes_one_dispatches_a_single_node(tmp_path, monkeypatch):
    # x-0ad6: the daemon's sequential fire-and-forget dispatch selects and spawns
    # exactly one node; below one lane still dispatches nothing.
    ready = _nodes(("n-a", "code"), ("n-c", "docs"))
    _wire(monkeypatch, tmp_path, ready)

    assert advance.dispatch_lanes(0, claims_root=tmp_path / "claims") == []

    receipts = advance.dispatch_lanes(
        1, project_root=tmp_path, claims_root=tmp_path / "claims"
    )
    assert [r["node_id"] for r in receipts] == ["n-a"]
    assert receipts[0]["status"] == "dispatched"
    assert active_lane_count(root=tmp_path / "claims") == 1


def test_empty_ready_dispatches_nothing(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, [])
    assert advance.dispatch_lanes(3, claims_root=tmp_path / "claims") == []


def test_dispatch_reservation_skips_node_already_being_dispatched(tmp_path, monkeypatch):
    """A concurrent sequential advance holds dispatch:<id>; the lane path (which
    the sequential path can't see via node:/dispatch:) must not double-launch."""
    from fno.claims.core import acquire_claim

    ready = _nodes(("n-a", "code"), ("n-c", "docs"))
    _wire(monkeypatch, tmp_path, ready)
    dkey = "dispatch:n-a"
    acquire_claim(dkey, "advance:other", ttl_ms=180_000, root=advance._claims_root_for(dkey))

    receipts = advance.dispatch_lanes(
        3, project_root=tmp_path, claims_root=tmp_path / "claims"
    )

    by_id = {r["node_id"]: r for r in receipts}
    assert by_id["n-a"]["status"] == "skipped"
    assert "already-claimed" in by_id["n-a"]["error"]
    assert by_id["n-c"]["status"] == "dispatched"
    # n-a's lane slot returned to the pool; only the good lane keeps one.
    assert find_lane_slot("n-a", root=tmp_path / "claims") is None
    assert active_lane_count(root=tmp_path / "claims") == 1


def test_seed_heals_symlinked_fno_before_writing(tmp_path):
    """A reused worktree's whole-dir `.fno` symlink must not route the seed into
    the canonical file (which would make every lane share one project.id)."""
    canonical = tmp_path / "canonical"
    (canonical / ".fno").mkdir(parents=True)
    (canonical / ".fno" / "config.local.toml").write_text("canonical = 'sentinel'\n")
    wt = tmp_path / "wt"
    wt.mkdir()
    (wt / ".fno").symlink_to(canonical / ".fno")

    advance._seed_lane_local_settings(wt, "n-a", "fno")

    assert (canonical / ".fno" / "config.local.toml").read_text() == "canonical = 'sentinel'\n"
    assert not (wt / ".fno").is_symlink()
    assert "fno-n-a" in (wt / ".fno" / "config.local.toml").read_text()


def test_ensure_heals_symlinked_fno_BEFORE_setup_runs(tmp_path, monkeypatch):
    """A reused worktree's `.fno` symlink must be healed before setup-worktree.sh,
    or setup links shared state through the symlink into canonical (codex P2)."""
    import types

    canonical = tmp_path / "canonical"
    (canonical / ".fno").mkdir(parents=True)
    wt = tmp_path / "wt"
    wt.mkdir()
    (wt / ".fno").symlink_to(canonical / ".fno")

    monkeypatch.setattr(
        advance.subprocess,
        "run",
        lambda *a, **k: types.SimpleNamespace(returncode=0, stdout=f"{wt}\n", stderr=""),
    )
    seen = {}

    def fake_setup(worktree, canonical_root):
        # setup must see a REAL dir, never the symlink.
        seen["is_symlink_at_setup"] = (worktree / ".fno").is_symlink()

    monkeypatch.setattr(advance, "_run_setup_worktree", fake_setup)

    out = advance._ensure_lane_worktree("n-a", canonical_root=canonical)

    assert out == wt
    assert seen["is_symlink_at_setup"] is False
    assert not (wt / ".fno").is_symlink()
