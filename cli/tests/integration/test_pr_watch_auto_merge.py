"""Integration: a parked granted worker's PR merges through the watcher.

The journey this file proves (end to end through the REAL canonical merge
core - only gh, git, the Rust verdict transport, and the coverage producer
are faked):

1. CI pending                              -> the merge core is invoked and
                                              HELD by the checks gate; no
                                              `gh pr merge` is called
2. CI turns green, the next drain runs     -> the guarded merge executes and
                                              the positive events name the
                                              actor, the grant session, the
                                              PR, and the node
3. the standing grant with a dead observer -> status reports
                                              observer_unavailable

The verdict arms themselves live in crates/fno-agents/src/merge_grant.rs;
these tests stub the transport to a granted verdict and prove the drain.
"""
from __future__ import annotations

from pathlib import Path

from fno.pr_watch._dispatch import run_execute_queue
from fno.pr_watch._discover import PrCandidate
from fno.pr_watch._state import WatermarkStore

from tests.unit.test_pr_merge import FakeRun  # reuse the exhaustive gh/git fake

NODE = "x-watch0001"
PR = 7
SLUG = "owner/repo"
GRANT = {"source": "config", "recorded_by": "spawner-session",
         "recorded_at": "2026-08-24T10:00:00Z"}


def _grant_verdict():
    from fno.pr._merge_grant import GRANTED, GrantVerdict

    return GrantVerdict(GRANTED, "newest durable grant approved", node_id=NODE,
                        claim_state="stale", grant=GRANT)


def _arm_world(
    monkeypatch,
    tmp_path: Path,
    *,
    checks_pending: bool,
) -> FakeRun:
    """The real merge core over a faked gh/git transport and a stubbed
    Rust verdict."""
    from fno.config import AutoMergeBlock
    import fno.pr._coverage_gate as coverage_gate
    import fno.pr._merge as merge_mod

    monkeypatch.setattr(
        "fno.pr._merge_grant.resolve_durable_grant",
        lambda pr, repo: _grant_verdict(),
    )
    monkeypatch.setattr(
        "fno.pr._review_hold.resolve_pr_worktree", lambda _pr, repo: repo
    )
    monkeypatch.setattr(merge_mod, "_load_auto_merge", lambda _repo: AutoMergeBlock(enabled=True))
    monkeypatch.setattr(merge_mod.shutil, "which", lambda _x: "/usr/bin/gh")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    # The journey is about grant/CI sequencing, not the coverage
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

    # The checks verdict, the head pin and the gh argv moved into the
    # authorized-merge owner, so this journey arms the OWNER rather than the
    # rollup above. The stub answers what the owner answers: pending checks
    # hold, green authorizes and merges. The rollup stays because the rest of
    # the transport still reads it.
    def _owner(pr_number, repo, *, effect, approved, source, **kwargs):
        if checks_pending:
            return {
                "outcome": "held",
                "detail": (
                    "checks are pending; require_checks_pass forbids merging "
                    "without green"
                ),
            }
        if kwargs.get("decide_only"):
            return {"outcome": "authorized", "detail": "cafe" * 6}
        return {"outcome": "merged", "detail": "cafe" * 6}

    monkeypatch.setattr(merge_mod, "_authorized_merge", _owner)
    return fake


def _candidate(tmp_path: Path) -> PrCandidate:
    return PrCandidate(
        node_id=NODE,
        pr_number=PR,
        pr_url=f"https://github.com/{SLUG}/pull/{PR}",
        repo_dir=tmp_path,
        repo_slug=SLUG,
    )


class _NullTickClaim:
    def acquire_tick_lock(self, key, holder): pass

    def release_tick_lock(self, key, holder): pass

    def acquire_pr_lock(self, key, holder): pass

    def release_pr_lock(self, key, holder): pass

    def is_node_live(self, node_id): return False


def _drain(tmp_path: Path, store_path: Path) -> list[dict]:
    """The merge phase's half of the journey: one granted queue row, drained."""
    events: list[dict] = []
    emit = lambda kind, data: events.append({"type": kind, "data": data})
    run_execute_queue(
        [(_candidate(tmp_path), f"{SLUG}#{PR}", dict(GRANT))],
        store_path=store_path, emit=emit,
        notify=lambda *a, **k: None, max_retries=3,
        claim=_NullTickClaim(),
    )
    return events


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
    def test_granted_drain_holds_on_pending_checks_then_merges_green(
        self, tmp_path, monkeypatch
    ):
        # (1) CI pending: the merge core runs and the checks gate HOLDS it -
        # no `gh pr merge` reaches the transport.
        _arm_world(monkeypatch, tmp_path, checks_pending=True)
        store = _seed_entry(tmp_path)
        events = _drain(tmp_path, store)
        reserved = _grant_events(events, "reserved")
        held = _grant_events(events, "held")
        assert len(reserved) == 1 and len(held) == 1
        assert not _grant_events(events, "executed")

        # (2) CI green, next drain: the guarded merge executes through the
        # canonical core, and the positive events name actor, grant session,
        # PR, and node.
        _arm_world(monkeypatch, tmp_path, checks_pending=False)
        events = _drain(tmp_path, store)
        executed = _grant_events(events, "executed")
        assert len(executed) == 1
        data = executed[0]["data"]
        assert data["actor"] == "pr-watch"
        assert data["pr"] == PR
        assert data["node_id"] == NODE
        assert data["recorded_by"] == "spawner-session"

    def test_dead_observer_reads_unavailable_in_the_status_projection(
        self, tmp_path, monkeypatch
    ):
        """AC12-ERR: a standing grant with a dead watcher is loud, with a
        repair, from the same receipt a human reads."""
        from fno.pr import _status

        monkeypatch.setattr(
            "fno.pr._merge_grant.resolve_durable_grant",
            lambda pr, repo: _grant_verdict(),
        )
        monkeypatch.setattr(
            "fno.pr_watch._install.liveness_report_live",
            lambda **kw: {"verdict": "disabled", "detail": "pr_watch.enabled=false",
                          "fix": ""},
        )

        projection = _status._merge_execution_projection(str(tmp_path), str(PR))

        assert projection["state"] == "granted"
        assert projection["observer"]["state"] == "observer_unavailable"
        assert projection["observer"]["repair"]
