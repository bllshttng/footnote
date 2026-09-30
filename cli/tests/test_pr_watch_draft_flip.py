"""The pr-watch draft flip leg (config.pr.open_ready's second enforcement point).

Self-contained on purpose: test_pr_watch_dispatch.py is over the file budget
and may only shrink, so the flip-leg tests live here with their own stubs.
One table test per surface; every branch keeps a row. No real gh, config,
or decision index is touched.
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


def _obs(pr_number=7, state="OPEN", is_draft=None):
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


def _run_tick(tmp_path, flip_fn, is_draft, **tick_kw):
    from fno.pr_watch._dispatch import tick

    events = []

    def discover(_entries):
        return [_candidate()]

    def read_state(cand, *, reviewers):
        return _obs(7, is_draft=is_draft)

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


@pytest.mark.parametrize(
    "is_draft,enabled,flip,expect_flips",
    [
        # the firing branch: OPEN + is_draft runs the leg, receipt counts it.
        (True, True, "ok", 1),
        # no draft bit, or the leg disabled by config: no flip, count 0.
        (None, True, "ok", 0),
        (True, False, "ok", 0),
        # a raising flip is one degraded row; the sweep completes past it.
        (True, True, "raises", 0),
    ],
)
def test_flip_leg_branches(tmp_path, is_draft, enabled, flip, expect_flips):
    store_path = tmp_path / "state.json"
    _seed_open(store_path)

    def boom(cand, obs, *, emit):
        raise RuntimeError("flip exploded")

    result, events = _run_tick(
        tmp_path,
        flip_fn=None if flip == "ok" else boom,
        is_draft=is_draft,
        draft_flip_enabled=enabled,
    )
    assert result.draft_flips == expect_flips
    receipt = next(d for t, d in events if t == "pr_watch_tick")
    assert receipt["draft_flips"] == expect_flips



