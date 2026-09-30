"""The pr-watch draft flip leg (config.pr.open_ready's second enforcement point).

The decision lives in the fno-agents binary (crates/fno-agents/src/
pr_draft_ready.rs); the tick is the forwarder. These tests stub
``fno.rust_binary.verb_call`` and pin the door payload; the guard's own
decision table is tested in the Rust crate. No real gh, config, or decision
index is touched.
"""
from __future__ import annotations

import pytest

from fno.pr_watch._discover import PrCandidate, PrObservation
from fno.pr_watch._state import WatermarkStore


@pytest.fixture(autouse=True)
def _free_gh_budget(monkeypatch):
    import fno.pr_watch._dispatch as _dispatch_mod

    monkeypatch.setattr(_dispatch_mod, "_gh_budget_backoff_left", lambda: 0.0)


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


def _run_tick(tmp_path, monkeypatch, verb_calls, is_draft, verb_raises=False, **tick_kw):
    import fno.rust_binary as rust_binary
    from fno.pr_watch._dispatch import tick

    def discover(_entries):
        return [_candidate()]

    def read_state(cand, *, reviewers):
        return _obs(7, is_draft=is_draft)

    def fake_verb(verb, payload, **kw):
        verb_calls.append((verb, payload, kw))
        if verb_raises:
            raise RuntimeError("door down")
        return {"outcome": "flipped", "receipt": "flipped", "journaled": True}

    monkeypatch.setattr(rust_binary, "verb_call", fake_verb)

    kw = dict(
        graph_path=tmp_path / "graph.json",
        store_path=tmp_path / "state.json",
        discover_fn=discover,
        read_pr_state_fn=read_state,
        read_tracked_states_fn=lambda keys: ({key: "OPEN" for key in keys}, 0),
        fire_skill_fn=lambda *a, **k: None,
        emit=lambda t, d: None,
        reviewers_for=lambda _d: [],
        claim=_Claim(),
        notify=lambda *a, **k: None,
        post_merge_readiness_fn=lambda _r: None,
        now_iso="2026-06-14T12:00:00Z",
    )
    kw.update(tick_kw)
    return tick(**kw)


def _seed_open(store_path, pr_number=7, now="2026-06-14T12:00:00Z"):
    WatermarkStore(path=store_path).set(f"owner/repo#{pr_number}", {
        "last_review_ts": now,
        "last_seen_state": "OPEN",
        "merge_dispatched": False,
        "retries": 0,
        "parked": None,
    })


@pytest.mark.parametrize(
    "is_draft,expect_calls",
    [
        # the firing branch: OPEN + is_draft forwards one door flip.
        (True, 1),
        # no draft bit (gh omitted it, or the PR is ready): the leg never runs.
        (None, 0),
        (False, 0),
    ],
)
def test_flip_leg_forwards_the_door_call(tmp_path, monkeypatch, is_draft, expect_calls):
    store_path = tmp_path / "state.json"
    _seed_open(store_path)
    calls = []
    _run_tick(tmp_path, monkeypatch, calls, is_draft)
    assert len(calls) == expect_calls
    if expect_calls:
        verb, payload, kw = calls[0]
        assert verb == "graph-get"
        flip = payload["pr_draft_ready"]["flip"]
        assert flip["pr"] == 7
        assert flip["repo"] == "owner/repo"
        assert flip["node"] == "x-abc12345"
        assert "journal" in flip, "the door journals to the tick's own events path"
        assert kw.get("timeout") == 60


def test_flip_failure_is_one_degraded_row(tmp_path, monkeypatch):
    store_path = tmp_path / "state.json"
    _seed_open(store_path)
    calls = []
    _run_tick(tmp_path, monkeypatch, calls, True, verb_raises=True)
    assert len(calls) == 1, "the tick completed past the failed flip"
