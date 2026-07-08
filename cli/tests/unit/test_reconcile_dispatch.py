"""Tests for G4 merge-triggered reconciliation dispatch (x-baeb).

Covers the router (dispatch_reconcile_for_blocker) + the pending-sentinel
(AC8 merge-before-manifest) + the manifest-write re-fire (fire_pending_reconcile).

Claim isolation mirrors test_advance: claims route under a tmp FNO_CLAIMS_ROOT +
FNO_REPO_ROOT, and the dependent's manifest lives under tmp_path/.fno so
`_dep_root` (cwd fallback) resolves there with no settings dependency.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno import stub_manifest as sm
from fno.backlog import reconcile_dispatch as rd
from fno.claims.core import acquire_claim


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")
    return tmp_path / ".fno" / "events.jsonl"


def _events(p: Path) -> list[dict]:
    if not p.exists():
        return []
    return [json.loads(l) for l in p.read_text().splitlines() if l.strip()]


def _dep(tmp_path: Path, node_id="x-dep"):
    return {"id": node_id, "project": None, "slug": "dep", "cwd": str(tmp_path)}


def _patch_deps(monkeypatch, deps):
    monkeypatch.setattr(rd, "_contract_dependents", lambda closed: deps)


def _patch_spawn(monkeypatch):
    calls = []
    def fake(node_id, node_cwd, node_slug=None, *, reconcile_manifest=None, model=None, provider=None):
        calls.append({"node": node_id, "cwd": node_cwd, "manifest": reconcile_manifest,
                      "model": model, "provider": provider})
        return "short123"
    monkeypatch.setattr(rd, "_spawn_worker", fake)
    return calls


# ---- router: dispatch / pending / skip ----

def test_unreconciled_manifest_dispatches_reconcile(iso, tmp_path, monkeypatch):
    # AC4-HP: a contract dependent with an unreconciled manifest gets a
    # /target --reconcile dispatch carrying the manifest path.
    sm.write("x-dep", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path,
             contract_test="true")
    _patch_deps(monkeypatch, [_dep(tmp_path)])
    calls = _patch_spawn(monkeypatch)

    res = rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)

    assert len(res) == 1 and res[0].decision == "dispatched"
    assert len(calls) == 1
    assert calls[0]["manifest"] == str(sm.manifest_path("x-dep", tmp_path))
    ev = _events(iso)
    assert [e for e in ev if e["type"] == "advance_dispatched"]


def test_reconcile_threads_node_model_pin(iso, tmp_path, monkeypatch):
    # x-571f (codex review PR #150): a pinned contract dependent's reconcile
    # worker must launch on the node's model, not the provider default.
    sm.write("x-dep", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path,
             contract_test="true")
    dep = _dep(tmp_path)
    dep["model"] = "fable"
    _patch_deps(monkeypatch, [dep])
    calls = _patch_spawn(monkeypatch)

    res = rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)

    assert res[0].decision == "dispatched"
    assert calls[0]["model"] == "fable"


def test_reconcile_threads_node_model_tier(iso, tmp_path, monkeypatch):
    # AC7 (x-da6e): a TIERED dependent (no model pin) must resolve its tier on
    # reconcile too -- the old raw `dep.get("model")` read passed None, silently
    # dropping the tier. Scoped to claude, the pick must map to the claude harness.
    from fno.adapters.providers import benchmarks as bm

    sm.write("x-dep", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path,
             contract_test="true")
    dep = _dep(tmp_path)
    dep["model_tier"] = "medium"
    dep["provider"] = "claude"
    _patch_deps(monkeypatch, [dep])
    calls = _patch_spawn(monkeypatch)

    res = rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)

    assert res[0].decision == "dispatched"
    picked = calls[0]["model"]
    assert picked is not None  # tier resolved, not dropped to None
    assert bm.reachable(picked)[0] == "claude"  # scoped to the claude lane
    # the worker must spawn on the SAME provider the model was resolved for,
    # else it is claude --model <foreign> (gemini HIGH / codex P2 on PR #258).
    assert calls[0]["provider"] == "claude"


def test_contract_dependents_copies_provider(monkeypatch, tmp_path):
    """AC7 (x-da6e): the dep dict must carry `provider` so reconcile scopes the
    tier and spawns on the same harness as the other dispatch paths."""
    from fno.backlog import reconcile_dispatch as rdmod

    graph = [
        {"id": "x-blk", "_status": "done"},
        {"id": "x-dep", "blocked_by": ["x-blk"], "dep": "contract",
         "provider": "claude", "model_tier": "medium"},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda _p: graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    deps = rdmod._contract_dependents("x-blk")
    assert deps and deps[0]["provider"] == "claude"


def test_missing_manifest_writes_pending_sentinel(iso, tmp_path, monkeypatch):
    # AC8: blocker merged before the dependent's first pass wrote its manifest ->
    # a reconcile:<dep> sentinel is reserved and NOTHING is dispatched.
    _patch_deps(monkeypatch, [_dep(tmp_path)])
    calls = _patch_spawn(monkeypatch)

    res = rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)

    assert res[0].decision == "skipped" and res[0].reason == "reconcile-pending"
    assert calls == []
    assert rd._sentinel_is_live("x-dep") is True


def test_reconciled_manifest_skips(iso, tmp_path, monkeypatch):
    sm.write("x-dep", [], tmp_path, contract_test="true", reconciled=True)
    _patch_deps(monkeypatch, [_dep(tmp_path)])
    calls = _patch_spawn(monkeypatch)

    res = rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)

    assert res[0].decision == "skipped" and res[0].reason == "already-reconciled"
    assert calls == []


def test_no_contract_dependents_is_noop(iso, tmp_path, monkeypatch):
    # Boundaries: a pure-hard close routes nothing here (advance owns it).
    _patch_deps(monkeypatch, [])
    calls = _patch_spawn(monkeypatch)
    assert rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso) == []
    assert _events(iso) == []
    assert calls == []


def test_disabled_dispatches_nothing(iso, tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "0")
    _patch_deps(monkeypatch, [_dep(tmp_path)])
    calls = _patch_spawn(monkeypatch)
    assert rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso) == []
    assert calls == []


def test_live_node_claim_dedups(iso, tmp_path, monkeypatch):
    # Invariant: at-most-one reconcile. A worker already owns node:<dep>.
    sm.write("x-dep", [], tmp_path, contract_test="true")
    acquire_claim("node:x-dep", "other-worker", ttl_ms=60_000,
                  root=rd._claims_root_for("node:x-dep"))
    _patch_deps(monkeypatch, [_dep(tmp_path)])
    calls = _patch_spawn(monkeypatch)

    res = rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)
    assert res[0].decision == "skipped" and res[0].reason == "already-claimed"
    assert calls == []


# ---- exactly-once across triggers ----

def test_second_trigger_does_not_redispatch(iso, tmp_path, monkeypatch):
    # AC: the same blocker observed twice dispatches the reconcile once (the
    # dispatch:<id> TTL reservation from the first run still covers the window).
    sm.write("x-dep", [], tmp_path, contract_test="true")
    _patch_deps(monkeypatch, [_dep(tmp_path)])
    calls = _patch_spawn(monkeypatch)

    rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)
    rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)
    assert len(calls) == 1


# ---- fire_pending_reconcile (the manifest-write re-fire) ----

def test_fire_pending_reconcile_dispatches_and_releases(iso, tmp_path, monkeypatch):
    # AC8: a pending sentinel + a now-written manifest fires the reconcile and
    # drops the sentinel. The graph lookup is bypassed (no node) so it falls back
    # to {id, cwd=root}; the manifest path is built from root.
    sm.write("x-dep", [], tmp_path, contract_test="true")
    acquire_claim("reconcile:x-dep", rd._pending_holder("x-dep"), ttl_ms=600_000,
                  root=rd._claims_root_for("reconcile:x-dep"))
    monkeypatch.setattr(rd, "_contract_dependents", lambda c: [])  # unused path guard
    calls = _patch_spawn(monkeypatch)
    # graph lookup inside fire_* will read the real graph; force the fallback.
    monkeypatch.setattr("fno.graph.store.read_graph", lambda *a, **k: [])

    res = rd.fire_pending_reconcile("x-dep", tmp_path)
    assert res is not None and res.decision == "dispatched"
    assert len(calls) == 1
    assert rd._sentinel_is_live("x-dep") is False  # released


def test_fire_pending_reconcile_noop_without_sentinel(iso, tmp_path, monkeypatch):
    sm.write("x-dep", [], tmp_path, contract_test="true")
    calls = _patch_spawn(monkeypatch)
    assert rd.fire_pending_reconcile("x-dep", tmp_path) is None
    assert calls == []
