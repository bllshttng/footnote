"""x-713b: the sweep bounds each candidate by the live phase clock.

A rich read never outlives the sweep slice, the cold post-merge ritual waits
for a slice that can hold it, and a cut row names the sweep sub-step it
caught. One test per plan acceptance criterion, AC1-AC7.

Lives apart from test_pr_watch_dispatch.py because that file is at the
5000-line budget and may only shrink.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace
from typing import Optional

import fno.pr_watch._dispatch as d
from fno.pr_watch._state import WatermarkStore


def _make_obs(
    pr_number: int = 1,
    state: str = "OPEN",
    latest_review_ts: Optional[str] = None,
):
    from fno.pr_watch._discover import PrObservation

    return PrObservation(
        pr_number=pr_number,
        state=state,
        latest_review_ts=latest_review_ts,
        opened_at="2026-06-01T00:00:00Z",
    )


def _make_candidate(
    node_id: str = "x-abc12345",
    pr_number: int = 1,
    repo_dir: Optional[Path] = None,
    repo_slug: str = "owner/repo",
):
    from fno.pr_watch._discover import PrCandidate

    return PrCandidate(
        node_id=node_id,
        pr_number=pr_number,
        pr_url=f"https://github.com/{repo_slug}/pull/{pr_number}",
        repo_dir=repo_dir,
        repo_slug=repo_slug,
    )


def _make_tick_deps(tmp_path: Path, candidates=None, obs_map: Optional[dict] = None):
    """Minimal injectable stubs for tick(); merge-ready by default so a
    MERGED observation decides as kind=merge."""
    from fno.pr_watch._dispatch import DispatchResult

    events_emitted: list[dict] = []
    fired: list[dict] = []
    if candidates is None:
        candidates = []

    def fake_discover(entries):
        return candidates

    def fake_read_pr_state(candidate, *, reviewers, runner=None, timeout_s=30.0):
        if obs_map and candidate.pr_number in obs_map:
            return obs_map[candidate.pr_number]
        return _make_obs(pr_number=candidate.pr_number)

    def fake_fire_skill(verb, pr_number, repo_dir, *, node_id=None, runner=None, model=None, env_seam=None):
        fired.append({"verb": verb, "pr": pr_number})
        return DispatchResult(ok=True, rc=0, is_error=False, raw='{"is_error":false}')

    def fake_emit(event_type: str, data: dict):
        events_emitted.append({"type": event_type, "data": data})

    class FakeClaim:
        def acquire_tick_lock(self, key, holder): pass
        def release_tick_lock(self, key, holder): pass
        def acquire_pr_lock(self, key, holder): pass
        def release_pr_lock(self, key, holder): pass
        def is_node_live(self, node_id): return False

    class V:
        is_ready = True

    return {
        "events": events_emitted,
        "fired": fired,
        "discover": fake_discover,
        "read_pr_state": fake_read_pr_state,
        "fire_skill": fake_fire_skill,
        "emit": fake_emit,
        "reviewers_for": lambda _: ["gemini-code-assist"],
        "claim": FakeClaim(),
        "notify": lambda *a, **kw: None,
        "post_merge_readiness": lambda repo_root: V(),
    }


class TestSweepBoundedByPhaseClock:
    @staticmethod
    def _listing(not_open: set):
        def read(keys):
            return {
                k: ("NOT_OPEN" if k in not_open else "MERGED") for k in keys
            }, 0

        return read

    @staticmethod
    def _seed_open_row(tmp_path: Path, *, pr=7):
        store_path = tmp_path / "state.json"
        WatermarkStore(path=store_path).set(f"owner/repo#{pr}", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })
        return store_path

    def _tick(self, tmp_path, deps, store_path, *, listing, deadline, ritual_fn=None):
        from fno.pr_watch._dispatch import tick

        return tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            read_tracked_states_fn=listing,
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
            graphql_remaining_fn=lambda: (4800, None),
            dispatch_deadline=deadline,
            dispatch_ritual_fn=ritual_fn,
        )

    def test_default_read_pr_state_bounded_by_the_phase_clock(self, monkeypatch):
        """AC1-HP: with 20s of phase left, the rich read's timeout keeps the
        slice's 10s persist reserve instead of outliving the alarm."""
        import time as _time

        import fno.pr_watch._discover as disc

        recorded: list[float] = []

        def fake_read(candidate, *, reviewers, timeout_s):
            recorded.append(timeout_s)
            return d._noop_read_state(candidate, reviewers=reviewers)

        monkeypatch.setattr(disc, "read_pr_state", fake_read, raising=True)
        d.set_phase_deadline(_time.monotonic() + 20)
        try:
            d._default_read_pr_state(_make_candidate(pr_number=1), reviewers=[])
        finally:
            d.set_phase_deadline(None)
        assert len(recorded) == 1
        assert 0 < recorded[0] <= 10.0

    def test_default_read_pr_state_keeps_30s_without_a_phase(self, monkeypatch):
        """AC2-EDGE: no phase armed, the read keeps today's 30s bound."""
        import fno.pr_watch._discover as disc

        recorded: list[float] = []

        def fake_read(candidate, *, reviewers, timeout_s):
            recorded.append(timeout_s)
            return d._noop_read_state(candidate, reviewers=reviewers)

        monkeypatch.setattr(disc, "read_pr_state", fake_read, raising=True)
        d._default_read_pr_state(_make_candidate(pr_number=1), reviewers=[])
        assert recorded == [30.0]

    def test_cold_ritual_fires_with_a_full_slice(self, tmp_path, monkeypatch):
        """AC3-HP: 150s of phase left holds the cold ritual; the merge
        dispatch runs once."""
        from fno.post_merge_route import PostMergeDispatchResult

        deps = _make_tick_deps(
            tmp_path,
            candidates=[_make_candidate(pr_number=7, repo_dir=tmp_path)],
            obs_map={7: _make_obs(7, "MERGED")},
        )
        store_path = self._seed_open_row(tmp_path)
        clock = {"t": 1000.0}
        monkeypatch.setattr(d, "time", SimpleNamespace(monotonic=lambda: clock["t"]))
        monkeypatch.setattr(d, "_phase_deadline", clock["t"] + 150.0)
        ritual_calls: list[int] = []

        def fake_ritual(cand, obs, fire):
            ritual_calls.append(cand.pr_number)
            return PostMergeDispatchResult(
                "dispatched", cand.pr_number, short_id="abcd1234", detail="cold",
            )

        res = self._tick(
            tmp_path, deps, store_path,
            listing=self._listing({"owner/repo#7"}),
            deadline=clock["t"] + 600.0, ritual_fn=fake_ritual,
        )

        assert ritual_calls == [7]
        assert res.acted == 1
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert [e["data"]["kind"] for e in dispatched] == ["merge"]

    def test_cold_ritual_held_back_under_the_ritual_floor(self, tmp_path, monkeypatch):
        """AC4-ERR: 60s of slice cannot hold a ritual that needs ~100s; the
        merge is skipped as fire-budget, burns no retry, and the scan still
        counts the candidate."""
        from fno.pr_watch._dispatch import _delivery_state_path

        deps = _make_tick_deps(
            tmp_path,
            candidates=[_make_candidate(pr_number=7, repo_dir=tmp_path)],
            obs_map={7: _make_obs(7, "MERGED")},
        )
        store_path = self._seed_open_row(tmp_path)
        clock = {"t": 1000.0}
        monkeypatch.setattr(d, "time", SimpleNamespace(monotonic=lambda: clock["t"]))
        monkeypatch.setattr(d, "_phase_deadline", clock["t"] + 60.0)
        ritual_calls: list[int] = []

        def fake_ritual(cand, obs, fire):
            ritual_calls.append(cand.pr_number)
            raise AssertionError("the ritual must not start under the floor")

        self._tick(
            tmp_path, deps, store_path,
            listing=self._listing({"owner/repo#7"}),
            deadline=clock["t"] + 600.0, ritual_fn=fake_ritual,
        )

        assert ritual_calls == []
        skipped = [
            e["data"].get("reason") for e in deps["events"]
            if e["type"] == "pr_watch_skipped"
        ]
        assert skipped == ["fire-budget"]
        assert not [
            e for e in deps["events"] if e["type"] == "pr_watch_dispatch_failed"
        ]
        rec = json.loads(_delivery_state_path(store_path).read_text()).get(
            "owner/repo#7", {},
        )
        assert rec.get("retries", 0) == 0
        tick_receipt = next(
            e["data"] for e in deps["events"] if e["type"] == "pr_watch_tick"
        )
        assert tick_receipt["merge_scan"] == {"completed": True, "scanned": 1}

    def test_review_fire_keeps_the_thirty_second_floor(self, tmp_path, monkeypatch):
        """AC5-EDGE: a review check at 60s of phase left still fires; only
        the merge ritual waits for the 100s floor."""
        deps = _make_tick_deps(
            tmp_path,
            candidates=[_make_candidate(pr_number=7, repo_dir=tmp_path)],
            obs_map={7: _make_obs(7, "OPEN", latest_review_ts="2026-06-14T11:00:00Z")},
        )
        store_path = self._seed_open_row(tmp_path)
        clock = {"t": 1000.0}
        monkeypatch.setattr(d, "time", SimpleNamespace(monotonic=lambda: clock["t"]))
        monkeypatch.setattr(d, "_phase_deadline", clock["t"] + 60.0)

        res = self._tick(
            tmp_path, deps, store_path,
            listing=self._listing(set()), deadline=clock["t"] + 600.0,
        )

        assert [f["verb"] for f in deps["fired"]] == ["check"]
        assert res.acted == 1

    def test_read_records_the_sweep_dispatch_sub_step(self, tmp_path):
        """AC6-HP: a rich read runs inside the sweep:dispatch step, so a cut
        there names the stall instead of a bare timeout."""
        deps = _make_tick_deps(
            tmp_path,
            candidates=[_make_candidate(pr_number=7, repo_dir=tmp_path)],
        )
        store_path = self._seed_open_row(tmp_path)
        steps: list[str] = []
        base_read = deps["read_pr_state"]

        def recording_read(candidate, **kw):
            steps.append(d.current_tick_phase())
            return base_read(candidate, **kw)

        deps["read_pr_state"] = recording_read
        self._tick(
            tmp_path, deps, store_path,
            listing=self._listing(set()), deadline=None,
        )
        assert steps == ["sweep:dispatch"]

    def test_scan_note_counts_failed_reads(self, tmp_path):
        """AC7-ERR: one failed read of two keeps its place in the cut note:
        scanned=1 of 2 read_failed=1."""
        from fno.graph._reconcile import ReconcileError

        d.SCAN_PROGRESS.pop("sweep", None)
        candidates = [
            _make_candidate(pr_number=n, repo_dir=tmp_path) for n in (1, 2)
        ]
        deps = _make_tick_deps(tmp_path, candidates=candidates)
        store_path = self._seed_open_row(tmp_path, pr=1)
        base_read = deps["read_pr_state"]

        def flaky_read(candidate, **kw):
            if candidate.pr_number == 1:
                raise ReconcileError("gh query failed")
            return base_read(candidate, **kw)

        deps["read_pr_state"] = flaky_read
        self._tick(
            tmp_path, deps, store_path,
            listing=self._listing(set()), deadline=None,
        )
        assert d.SCAN_PROGRESS["sweep"] == "scanned=1 of 2 read_failed=1"
        delivery = WatermarkStore(path=d._delivery_state_path(store_path)).load()
        assert delivery["owner/repo#1"]["last_read_failed"] is True
        d.SCAN_PROGRESS.pop("sweep", None)  # module global: leave it as found

    def test_failed_read_cursor_yields_to_unpolled_candidates(self, tmp_path, monkeypatch):
        """A rich-read failure advances the next tick past untouched candidates."""
        from types import SimpleNamespace

        import pytest
        from fno.graph._reconcile import ReconcileError

        candidates = [_make_candidate(pr_number=n, repo_dir=tmp_path) for n in (1, 2, 3)]
        deps = _make_tick_deps(tmp_path, candidates=candidates)
        store_path = tmp_path / "state.json"
        prior = "2026-06-14T11:00:00Z"
        for pr in (2, 3):
            WatermarkStore(path=store_path).set(f"owner/repo#{pr}", {
                "last_review_ts": None, "last_seen_state": "OPEN",
                "merge_dispatched": False, "retries": 0, "parked": None,
                "last_polled_at": prior,
            })

        clock = {"t": 1000.0}
        monkeypatch.setattr(d, "time", SimpleNamespace(monotonic=lambda: clock["t"]))
        reads: list[int] = []
        fail = {"once": True}

        def flaky_read(candidate, **kw):
            reads.append(candidate.pr_number)
            clock["t"] += 1.0
            if candidate.pr_number == 1 and fail["once"]:
                fail["once"] = False
                raise ReconcileError("gh query timed out")
            return _make_obs(pr_number=candidate.pr_number)

        deps["read_pr_state"] = flaky_read
        self._tick(
            tmp_path, deps, store_path, listing=self._listing(set()),
            deadline=clock["t"] + 15.5,
        )
        assert reads == [1]
        assert "owner/repo#1" not in WatermarkStore(path=store_path).load()
        delivery = WatermarkStore(path=d._delivery_state_path(store_path)).load()
        assert delivery["owner/repo#1"] == {
            "last_polled_at": "2026-06-14T12:00:00Z",
            "last_read_failed": True,
        }

        reads.clear()
        self._tick(tmp_path, deps, store_path, listing=self._listing(set()), deadline=None)
        assert reads == [2, 3, 1]

    def test_phase_cut_persists_attempted_candidate_cursors(self, tmp_path, monkeypatch):
        """The watermark survives a cut raised inside a later candidate read."""
        from types import SimpleNamespace

        import pytest

        candidates = [_make_candidate(pr_number=n, repo_dir=tmp_path) for n in (1, 2, 3)]
        deps = _make_tick_deps(tmp_path, candidates=candidates)
        store_path = tmp_path / "state.json"
        prior = "2026-06-14T11:00:00Z"
        for pr in (1, 2, 3):
            WatermarkStore(path=store_path).set(f"owner/repo#{pr}", {
                "last_review_ts": None, "last_seen_state": "OPEN",
                "merge_dispatched": False, "retries": 0, "parked": None,
                "last_polled_at": prior,
            })

        clock = {"t": 1000.0}
        monkeypatch.setattr(d, "time", SimpleNamespace(monotonic=lambda: clock["t"]))
        reads: list[int] = []
        cut = {"once": True}

        def cutting_read(candidate, **kw):
            reads.append(candidate.pr_number)
            clock["t"] += 1.0
            if candidate.pr_number == 2 and cut["once"]:
                cut["once"] = False
                raise TimeoutError("phase cut")
            return _make_obs(pr_number=candidate.pr_number)

        deps["read_pr_state"] = cutting_read
        with pytest.raises(TimeoutError, match="phase cut"):
            self._tick(tmp_path, deps, store_path, listing=self._listing(set()), deadline=None)
        delivery = WatermarkStore(path=d._delivery_state_path(store_path)).load()
        assert delivery["owner/repo#1"]["last_polled_at"] == "2026-06-14T12:00:00Z"
        assert delivery["owner/repo#2"]["last_polled_at"] == "2026-06-14T12:00:00Z"

        reads.clear()
        self._tick(tmp_path, deps, store_path, listing=self._listing(set()), deadline=None)
        assert reads[0] == 3
