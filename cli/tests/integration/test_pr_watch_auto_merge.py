"""Integration: a parked granted worker's PR merges through the watcher.

The journey this file proves (the plan's wave-5 journey, end to end through
the REAL tick orchestrator and the REAL canonical merge core - only gh, git,
the claim probe, and the coverage producer are faked):

1. the worker is LIVE on the node          -> the watcher never executes
2. the worker parks (claim goes stale),
   CI pending                              -> the merge core is invoked and
                                              HELD by the checks gate; no
                                              `gh pr merge` is called
3. CI turns green, the next tick runs      -> the guarded merge executes and
                                              the positive events name the
                                              actor, the grant session, the
                                              PR, and the node
4. a newer `--no-merge` re-dispatch        -> the newest refusal outranks the
   (a refused receipt)                        older grant; nothing executes
5. the standing grant with a dead observer -> status reports
                                              observer_unavailable
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import pytest

from fno.pr_watch._dispatch import tick
from fno.pr_watch._discover import PrCandidate, PrObservation
from fno.pr_watch._state import WatermarkStore

from tests.unit.test_pr_merge import FakeRun  # reuse the exhaustive gh/git fake

NODE = "x-watch0001"
PR = 7
SLUG = "owner/repo"
NOW = "2026-08-24T12:00:00Z"


def _receipt(approved: bool, at: str) -> dict:
    return {
        "approved": approved,
        "source": "no-merge-flag" if not approved else "config",
        "recorded_by": "spawner-session",
        "recorded_at": at,
    }


def _write_graph(tmp_path: Path, receipts: list[dict]) -> None:
    sessions = [
        {"phase": "do", "harness": "claude", "session_id": f"w{i}",
         "merge_grant": receipt}
        for i, receipt in enumerate(receipts)
    ]
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{
        "id": NODE, "title": "t", "pr_number": PR, "sessions": sessions,
    }]}), encoding="utf-8")
    import fno.paths as paths_mod

    paths_mod.graph_json = lambda: g  # the resolver's own read
    import fno.pr._coverage_gate as cg

    cg._repo_slug = lambda repo: None  # bare-number node matching


def _arm_world(
    monkeypatch,
    tmp_path: Path,
    *,
    claim_state: str,
    checks_pending: bool,
) -> FakeRun:
    """The real merge core over a faked gh/git transport and claim probe."""
    from fno.config import AutoMergeBlock
    import fno.claims.core as claims_core
    import fno.config as config_mod
    import fno.pr._coverage_gate as coverage_gate
    import fno.pr._merge as merge_mod

    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, **kw: {"key": key, "state": claim_state, "holder": "w0"},
    )
    monkeypatch.setattr(
        "fno.config.load_settings_for_repo",
        lambda path: config_mod.load_settings().model_copy(
            update={"auto_merge": AutoMergeBlock(enabled=True, grant="dispatch")}
        ),
    )
    monkeypatch.setattr(merge_mod, "_load_auto_merge", lambda: AutoMergeBlock(enabled=True))
    monkeypatch.setattr(merge_mod.shutil, "which", lambda _x: "/usr/bin/gh")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    # The journey is about grant/claim/CI sequencing, not the coverage
    # producer (owned by the coverage suite): serve a covered row at the
    # merge gate's own seam.
    monkeypatch.setattr(
        coverage_gate,
        "coverage_verdict",
        lambda pr, repo, recompute=False, head=None: (
            coverage_gate.COVERED, "", "", "",
        ),
    )
    # Reuse-over-reimplement, and merge-blocking test seams from the unit
    # suite keep this journey about the watcher.
    monkeypatch.setattr(merge_mod, "_review_lane_configured", lambda repo, pr_number=0: True)
    monkeypatch.setattr(
        merge_mod, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    monkeypatch.setattr(
        "fno.pr._reviews._override_label_actor", lambda pr, repo, r: (False, None)
    )
    monkeypatch.setattr(
        "fno.pr._reviews.publish_coverage_status",
        lambda pr, head=None, cwd=None, repo=None, gate_verdict=None: (True, ""),
    )
    rollup = [
        {"name": "ci", "status": "COMPLETED",
         "conclusion": "PENDING" if checks_pending else "SUCCESS"},
    ]
    fake = FakeRun(
        gh_merge=None,
        toplevel=str(tmp_path),
        checks={"state": "OPEN", "headRefOid": "cafe" * 6, "statusCheckRollup": rollup},
    )
    monkeypatch.setattr(merge_mod, "run", fake)
    return fake


def _candidate(tmp_path: Path) -> PrCandidate:
    return PrCandidate(
        node_id=NODE,
        pr_number=PR,
        pr_url=f"https://github.com/{SLUG}/pull/{PR}",
        repo_dir=tmp_path,
        repo_slug=SLUG,
    )


def _obs() -> PrObservation:
    return PrObservation(
        pr_number=PR, state="OPEN", latest_review_ts=None, opened_at=NOW,
    )


def _run_tick(tmp_path: Path, store_path: Path) -> list[dict]:
    events: list[dict] = []
    tick(
        graph_path=tmp_path / "graph.json",
        store_path=store_path,
        discover_fn=lambda entries: [_candidate(tmp_path)],
        read_pr_state_fn=lambda cand, *, reviewers, runner=None, timeout_s=30.0: _obs(),
        read_tracked_states_fn=lambda keys: ({k: "OPEN" for k in keys}, 0),
        fire_skill_fn=lambda *a, **k: None,
        emit=lambda kind, data: events.append({"type": kind, "data": data}),
        reviewers_for=lambda repo_dir: [],
        claim=_NullTickClaim(),
        notify=lambda *a, **k: None,
        post_merge_readiness_fn=lambda repo_root: None,
        now_iso=NOW,
        max_retries=3,
        graphql_remaining_fn=lambda: (4800, None),
    )
    return events


class _NullTickClaim:
    def acquire_tick_lock(self, key, holder): pass

    def release_tick_lock(self, key, holder): pass

    def acquire_pr_lock(self, key, holder): pass

    def release_pr_lock(self, key, holder): pass

    def is_node_live(self, node_id): return False


def _seed_entry(tmp_path: Path) -> Path:
    store_path = tmp_path / "state.json"
    WatermarkStore(path=store_path).set(f"{SLUG}#{PR}", {
        "last_review_ts": None,
        "last_seen_state": "OPEN",
        "merge_dispatched": False,
        "retries": 0,
        "parked": None,
    })
    return store_path


def _grant_events(events: list[dict], phase: str) -> list[dict]:
    return [e for e in events
            if e["type"] == "merge_grant_execution" and e["data"]["phase"] == phase]


class TestParkedWorkerJourney:
    def test_live_hold_green_merge_refusal_outranks(self, tmp_path, monkeypatch):
        # The grant is on the node from a dispatch whose receipt is newest.
        _write_graph(tmp_path, [_receipt(True, "2026-08-24T10:00:00Z")])

        # (1) Worker LIVE: the tick must not even reserve an attempt.
        _arm_world(monkeypatch, tmp_path, claim_state="live", checks_pending=True)
        store = _seed_entry(tmp_path)
        events = _run_tick(tmp_path, store)
        assert not _grant_events(events, "reserved")
        assert not _grant_events(events, "executed")

        # (2) Worker parked (stale), CI pending: the merge core runs and the
        # checks gate HOLDS it - no `gh pr merge` reaches the transport.
        _arm_world(monkeypatch, tmp_path, claim_state="stale", checks_pending=True)
        events = _run_tick(tmp_path, store)
        reserved = _grant_events(events, "reserved")
        held = _grant_events(events, "held")
        assert len(reserved) == 1 and len(held) == 1
        assert not _grant_events(events, "executed")

        # (3) CI green, next tick: the guarded merge executes through the
        # canonical core, and the positive events name actor, grant session,
        # PR, and node.
        _arm_world(monkeypatch, tmp_path, claim_state="stale", checks_pending=False)
        events = _run_tick(tmp_path, store)
        executed = _grant_events(events, "executed")
        assert len(executed) == 1
        data = executed[0]["data"]
        assert data["actor"] == "pr-watch"
        assert data["pr"] == PR
        assert data["node_id"] == NODE
        assert data["recorded_by"] == "spawner-session"

    def test_newer_no_merge_refusal_outranks_older_grant(self, tmp_path, monkeypatch):
        """AC9-EDGE in the journey: a newer `--no-merge` re-dispatch records
        approved=false, and the watcher never merges despite the older grant."""
        _write_graph(tmp_path, [
            _receipt(True, "2026-08-24T10:00:00Z"),
            _receipt(False, "2026-08-24T11:00:00Z"),
        ])
        _arm_world(monkeypatch, tmp_path, claim_state="stale", checks_pending=False)
        store = _seed_entry(tmp_path)

        events = _run_tick(tmp_path, store)

        assert not _grant_events(events, "reserved")
        assert not _grant_events(events, "executed")

    def test_dead_observer_reads_unavailable_in_the_status_projection(
        self, tmp_path, monkeypatch
    ):
        """AC12-ERR: a standing grant with a dead watcher is loud, with a
        repair, from the same receipt a human reads."""
        from fno.pr import _status

        _write_graph(tmp_path, [_receipt(True, "2026-08-24T10:00:00Z")])
        _arm_world(monkeypatch, tmp_path, claim_state="stale", checks_pending=False)
        monkeypatch.setattr(
            "fno.pr_watch._install.liveness_report_live",
            lambda **kw: {"verdict": "disabled", "detail": "pr_watch.enabled=false",
                          "fix": ""},
        )

        projection = _status._merge_execution_projection(str(tmp_path), str(PR))

        assert projection["state"] == "granted"
        assert projection["observer"]["state"] == "observer_unavailable"
        assert projection["observer"]["repair"]
