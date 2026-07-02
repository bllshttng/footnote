"""End-to-end mechanical test of the G4 reconcile seam (x-baeb, cv-21ea5acd).

The `/target --reconcile` WORKER itself is LLM-interpreted (the de-stub is a real
implementation pass, not a unit-testable swap). But the mechanical chain the
worker drives - merge-held draft PR -> dispatch -> drift gate -> finalize -> no
longer held - must stay coherent. This test wires the REAL pieces together
(real graph read by the router, real merge guard, real verdict + finalize) so a
break anywhere in the seam is caught, even though the de-stub reasoning is not.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno import stub_manifest as sm
from fno.backlog import reconcile_dispatch as rd


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")
    return tmp_path / ".fno" / "events.jsonl"


def test_full_reconcile_seam_held_to_unheld(iso, tmp_path, monkeypatch):
    # --- world: a merged blocker + a contract dependent with a draft PR #42 ---
    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-blk", "completed_at": "2026-06-26T00:00:00Z"},
        {"id": "x-dep", "dep": "contract", "blocked_by": ["x-blk"],
         "pr_number": 42, "project": None, "cwd": str(tmp_path), "slug": "dep"},
    ]}), encoding="utf-8")
    # The router and the merge guard both read graph_json() internally.
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)

    # The dependent's first pass wrote its manifest: unreconciled, passing suite.
    sm.write("x-dep", [{"stub_id": "createUser", "file": "api.ts", "kind": "function"}],
             tmp_path, contract_version=1, contract_ref="d.md#ic", contract_test="true")

    # 1. BEFORE reconcile: merging #42 would ship mocks -> guard HOLDS.
    held = sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp)
    assert held is not None and held["_node"] == "x-dep"

    # 2. Blocker merge -> the router dispatches a /target --reconcile worker
    #    carrying the manifest path (real _contract_dependents graph read).
    calls = []
    monkeypatch.setattr(rd, "_spawn_worker", lambda n, c, s=None, *, reconcile_manifest=None, model=None:
                        calls.append(reconcile_manifest) or "short1")
    res = rd.dispatch_reconcile_for_blocker(closed_node_id="x-blk", events_path=iso)
    assert [r.decision for r in res] == ["dispatched"]
    assert calls == [str(sm.manifest_path("x-dep", tmp_path))]

    # 3. The worker pulls main + runs the drift gate -> authorize (suite passes).
    assert sm.reconcile_verdict("x-dep", tmp_path)["outcome"] == sm.AUTHORIZE

    # 4. The worker de-stubs (LLM, not tested here) then finalizes.
    sm.mark_reconciled("x-dep", tmp_path)

    # 5. AFTER reconcile: the guard no longer holds -> #42 is mergeable.
    assert sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp) is None


def test_full_seam_drift_keeps_pr_held(iso, tmp_path, monkeypatch):
    # The mirror path: a FAILING contract test refuses de-stub, so the guard
    # stays held and the draft PR can never merge with mocks (AC4-ERR end to end).
    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-dep", "dep": "contract", "blocked_by": ["x-blk"],
         "pr_number": 42, "project": None, "cwd": str(tmp_path), "slug": "dep"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    sm.write("x-dep", [{"stub_id": "a", "file": "f", "kind": "fn"}], tmp_path,
             contract_test="false")  # landed schema fails the gate

    # The worker's gate refuses; it must NOT call mark_reconciled.
    assert sm.reconcile_verdict("x-dep", tmp_path)["outcome"] == sm.DRIFT
    # Guard still holds -> the draft PR stays unmergeable.
    assert sm.unreconciled_manifest_for_pr(42, tmp_path, graph_path=gp) is not None
