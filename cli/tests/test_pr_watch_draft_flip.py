"""The pr-watch draft flip leg (config.pr.open_ready's second enforcement point).

Self-contained on purpose: `test_pr_watch_dispatch.py` is over the file
budget and may only shrink, so the flip-leg tests live here with their own
stubs. No real gh, config, or decision index is touched.
"""
from __future__ import annotations

import pytest

from fno.pr_watch._discover import PrCandidate, PrObservation
from fno.pr_watch._state import WatermarkStore


@pytest.fixture(autouse=True)
def _free_gh_budget(monkeypatch):
    import fno.pr_watch._dispatch as _dispatch_mod

    monkeypatch.setattr(_dispatch_mod, "_gh_budget_backoff_left", lambda: 0.0)


@pytest.fixture(autouse=True)
def _sandbox_bounce_receipts(tmp_path, monkeypatch):
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path / "state")


def _candidate(pr_number=7, slug="owner/repo", node_id="x-abc12345"):
    return PrCandidate(
        node_id=node_id,
        pr_number=pr_number,
        pr_url=f"https://github.com/{slug}/pull/{pr_number}",
        repo_dir=None,
        repo_slug=slug,
    )


def _obs(pr_number=7, state="OPEN", is_draft=True):
    return PrObservation(
        pr_number=pr_number,
        state=state,
        latest_review_ts=None,
        opened_at="2026-06-01T00:00:00Z",
        is_draft=is_draft,
    )


class _Claim:
    def acquire_tick_lock(self, key, holder):
        pass

    def release_tick_lock(self, key, holder):
        pass

    def acquire_pr_lock(self, key, holder):
        pass

    def release_pr_lock(self, key, holder):
        pass

    def is_node_live(self, node_id):
        return False


def _run_tick(tmp_path, candidates, obs_map, flip_fn, **tick_kw):
    from fno.pr_watch._dispatch import tick

    events = []

    def discover(_entries):
        return candidates

    def read_state(cand, *, reviewers):
        return obs_map.get(cand.pr_number) or _obs(cand.pr_number, is_draft=None)

    def fake_flip(cand, obs, *, emit):
        events.append(("flipped", cand.pr_number))
        return "flipped"

    kw = dict(
        graph_path=tmp_path / "graph.json",
        store_path=tmp_path / "state.json",
        discover_fn=discover,
        read_pr_state_fn=read_state,
        read_tracked_states_fn=lambda keys: ({key: "OPEN" for key in keys}, 0),
        fire_skill_fn=lambda *a, **k: None,
        emit=lambda t, d: events.append((t, d)),
        reviewers_for=lambda _d: [],
        claim=_Claim(),
        notify=lambda *a, **k: None,
        post_merge_readiness_fn=lambda _r: None,
        draft_flip_fn=flip_fn or fake_flip,
        now_iso="2026-06-14T12:00:00Z",
    )
    kw.update(tick_kw)
    return tick(**kw), events


def _seed_open(store_path, pr_number=7, now="2026-06-14T12:00:00Z"):
    WatermarkStore(path=store_path).set(f"owner/repo#{pr_number}", {
        "last_review_ts": now,
        "last_seen_state": "OPEN",
        "merge_dispatched": False,
        "retries": 0,
        "parked": None,
    })


def test_open_draft_candidate_is_flipped_and_counted(tmp_path):
    store_path = tmp_path / "state.json"
    _seed_open(store_path)
    result, events = _run_tick(tmp_path, [_candidate()], {7: _obs(7)}, None)
    assert ("flipped", 7) in events, "the flip leg must run for an OPEN + is_draft observation"
    assert result.draft_flips == 1
    receipt = next(d for t, d in events if t == "pr_watch_tick")
    assert receipt["draft_flips"] == 1


def test_non_draft_and_unknown_draft_observations_never_flip(tmp_path):
    store_path = tmp_path / "state.json"
    _seed_open(store_path, pr_number=7)
    calls = []
    result, events = _run_tick(
        tmp_path, [_candidate()], {7: _obs(7, is_draft=None)},
        flip_fn=lambda c, o, *, emit: calls.append(o),
    )
    assert calls == []
    assert result.draft_flips == 0


def test_flip_disabled_by_config_runs_no_leg(tmp_path):
    store_path = tmp_path / "state.json"
    _seed_open(store_path)
    calls = []
    result, _ = _run_tick(
        tmp_path, [_candidate()], {7: _obs(7)},
        flip_fn=lambda c, o, *, emit: calls.append(o),
        draft_flip_enabled=False,
    )
    assert calls == []
    assert result.draft_flips == 0


def test_a_raising_flip_never_breaks_the_sweep(tmp_path):
    store_path = tmp_path / "state.json"
    _seed_open(store_path)

    def boom(cand, obs, *, emit):
        raise RuntimeError("flip exploded")

    result, events = _run_tick(tmp_path, [_candidate()], {7: _obs(7)}, flip_fn=boom)
    assert result.acted >= 0, "the tick completes past a raising flip leg"
    receipt = next(d for t, d in events if t == "pr_watch_tick")
    assert receipt["merge_scan"]["completed"] is True
