"""Tests for pr_watch._state (watermark store) and pr_watch._dispatch (tick orchestrator).

TDD: tests written BEFORE implementation.  Every test targets a named
acceptance criterion from the task 1.2 spec.

Dependency injection is used throughout: no real claude, gh, launchd, or
filesystem-global writes.  Temporary directories replace ~/.fno.
"""
from __future__ import annotations

import json
import os
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Literal, Optional
from unittest.mock import MagicMock, call, patch

import pytest


@pytest.fixture(autouse=True)
def _hermetic_post_merge(monkeypatch):
    """Keep tick tests hermetic after the verb-first cutover.

    The default ``_default_dispatch_ritual`` now shells ``fno pr ritual`` and
    appends receipts to the global events log. Tests that exercise the real
    default dispatch (those that do NOT inject ``dispatch_ritual_fn``) would
    otherwise spawn a subprocess and pollute ~/.fno/events.jsonl. Redirect both
    to no-ops; tests injecting ``dispatch_ritual_fn`` never reach them.
    """
    import fno.post_merge_route as pmr

    monkeypatch.setattr(
        pmr, "_default_run_ritual_verb",
        lambda pr, cwd: pmr.ColdRitualResult(ok=True, tail="ok"),
    )
    monkeypatch.setattr(pmr, "emit_receipt", lambda *a, **k: True)


# ---------------------------------------------------------------------------
# Helpers / stubs
# ---------------------------------------------------------------------------


def _make_obs(
    pr_number: int = 1,
    state: str = "OPEN",
    merged: bool = False,  # kept for call-site compat; ignored (field removed)
    latest_review_ts: Optional[str] = None,
    opened_at: str = "2026-06-01T00:00:00Z",
):
    """Build a PrObservation without importing (tests run before impl exists)."""
    from fno.pr_watch._discover import PrObservation

    return PrObservation(
        pr_number=pr_number,
        state=state,
        latest_review_ts=latest_review_ts,
        opened_at=opened_at,
    )


def _arm_auto_run(repo_dir: Path) -> None:
    """Arm post_merge.auto_run for a repo dir. _default_dispatch_ritual honors it
    (x-7930): a bare tmp_path defaults auto_run off -> dispatch is 'disabled'."""
    cfg = repo_dir / ".fno" / "config.toml"
    cfg.parent.mkdir(parents=True, exist_ok=True)
    cfg.write_text("[post_merge]\nauto_run = true\n")


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


def _claude_ok_response(text: str = "done") -> subprocess.CompletedProcess:
    """Simulate a successful claude --print --output-format json response."""
    payload = json.dumps({"result": text, "is_error": False})
    return subprocess.CompletedProcess(args=[], returncode=0, stdout=payload, stderr="")


def _claude_is_error_response() -> subprocess.CompletedProcess:
    """rc=0 but is_error:true -- the load-bearing AC1-FR case."""
    payload = json.dumps({"result": "skill errored", "is_error": True})
    return subprocess.CompletedProcess(args=[], returncode=0, stdout=payload, stderr="")


def _claude_nonzero_response(rc: int = 1) -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=rc, stdout="", stderr="error")


# ---------------------------------------------------------------------------
# State module tests
# ---------------------------------------------------------------------------


class TestWatermarkStore:
    """AC: atomic watermark store round-trips and degrades on corruption."""

    def test_load_missing_returns_empty(self, tmp_path):
        """AC-HP: missing file -> load() returns {} without raising."""
        from fno.pr_watch._state import WatermarkStore

        store = WatermarkStore(path=tmp_path / "pr-watcher-state.json")
        assert store.load() == {}

    def test_set_and_get_round_trip(self, tmp_path):
        """AC-HP: set() persists; get() retrieves the same dict."""
        from fno.pr_watch._state import WatermarkStore

        store = WatermarkStore(path=tmp_path / "pr-watcher-state.json")
        entry = {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        }
        store.set("owner/repo#1", entry)
        assert store.get("owner/repo#1") == entry

    def test_atomic_persist_via_os_replace(self, tmp_path):
        """AC-VERIFY: persisted JSON is valid and contains the expected key."""
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "pr-watcher-state.json"
        store = WatermarkStore(path=path)
        store.set("owner/repo#42", {"last_seen_state": "MERGED", "merge_dispatched": True, "retries": 0, "parked": None, "last_review_ts": None})
        raw = json.loads(path.read_text())
        assert "owner/repo#42" in raw
        assert raw["owner/repo#42"]["merge_dispatched"] is True

    def test_corrupt_json_returns_empty_no_raise(self, tmp_path):
        """AC-ERR: corrupt JSON file -> load() returns {} and logs warning."""
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "pr-watcher-state.json"
        path.write_text("NOT VALID JSON {{{")
        store = WatermarkStore(path=path)
        result = store.load()
        assert result == {}

    def test_missing_repo_slug_fallback_key(self, tmp_path):
        """AC-EDGE: None repo_slug falls back to str(pr_number) as the key."""
        from fno.pr_watch._state import WatermarkStore, make_watermark_key

        key = make_watermark_key(repo_slug=None, pr_number=99)
        assert key == "99"

    def test_slug_key_format(self, tmp_path):
        """AC-HP: normal slug key = 'owner/repo#N'."""
        from fno.pr_watch._state import make_watermark_key

        key = make_watermark_key(repo_slug="owner/repo", pr_number=7)
        assert key == "owner/repo#7"


# ---------------------------------------------------------------------------
# fire_skill tests
# ---------------------------------------------------------------------------


class TestFireSkill:
    """AC1-FR: fire_skill honours rc=0+is_error:true as FAILURE."""

    def test_rc0_is_error_false_is_success(self, tmp_path):
        """AC-HP: rc=0, is_error=False -> DispatchResult.ok True."""
        from fno.pr_watch._dispatch import fire_skill

        def stub_runner(cmd, **kw):
            return _claude_ok_response()

        result = fire_skill("check", 1, tmp_path, runner=stub_runner)
        assert result.ok is True
        assert result.is_error is False
        assert result.rc == 0

    def test_rc0_is_error_true_is_failure(self, tmp_path):
        """AC1-FR (load-bearing): rc=0 but is_error:true -> DispatchResult.ok False."""
        from fno.pr_watch._dispatch import fire_skill

        def stub_runner(cmd, **kw):
            return _claude_is_error_response()

        result = fire_skill("check", 1, tmp_path, runner=stub_runner)
        assert result.ok is False
        assert result.is_error is True
        assert result.rc == 0

    def test_nonzero_rc_is_failure(self, tmp_path):
        """AC-ERR: non-zero rc -> DispatchResult.ok False."""
        from fno.pr_watch._dispatch import fire_skill

        def stub_runner(cmd, **kw):
            return _claude_nonzero_response(rc=2)

        result = fire_skill("check", 1, tmp_path, runner=stub_runner)
        assert result.ok is False
        assert result.rc == 2

    def test_unparseable_json_is_failure(self, tmp_path):
        """AC-ERR: stdout not JSON -> DispatchResult.ok False."""
        from fno.pr_watch._dispatch import fire_skill

        def stub_runner(cmd, **kw):
            return subprocess.CompletedProcess(args=[], returncode=0, stdout="not json", stderr="")

        result = fire_skill("check", 1, tmp_path, runner=stub_runner)
        assert result.ok is False

    def test_env_seam_overrides_command(self, tmp_path, monkeypatch):
        """AC-EDGE: PR_WATCH_FIRE_CMD env seam overrides the real claude invocation."""
        from fno.pr_watch._dispatch import fire_skill

        captured = {}

        def stub_runner(cmd, **kw):
            captured["cmd"] = cmd
            return _claude_ok_response()

        monkeypatch.setenv("PR_WATCH_FIRE_CMD", "true")
        result = fire_skill("check", 5, tmp_path, runner=stub_runner)
        # When seam is set, the command prefix should change (stub runner sees it)
        assert result.ok is True

    def test_check_verb_fires_correct_skill(self, tmp_path):
        """AC-HP: verb='check' -> /fno:pr check <n> in command."""
        from fno.pr_watch._dispatch import fire_skill

        captured = {}

        def stub_runner(cmd, **kw):
            captured["cmd"] = cmd
            return _claude_ok_response()

        fire_skill("check", 7, tmp_path, runner=stub_runner)
        cmd_str = " ".join(str(c) for c in captured["cmd"])
        assert "check" in cmd_str
        assert "7" in cmd_str
        # `autonomous` is merged-only; check must not carry it.
        assert "autonomous" not in cmd_str

    def test_runner_receives_bounded_timeout(self, tmp_path):
        """x-97d8: fire_skill MUST pass a bounded timeout= to the runner so a
        wedged headless claude cannot block the tick forever."""
        from fno.pr_watch._dispatch import fire_skill

        captured = {}

        def stub_runner(cmd, **kw):
            captured.update(kw)
            return _claude_ok_response()

        fire_skill("check", 7, tmp_path, runner=stub_runner)
        assert captured.get("timeout") is not None
        assert captured["timeout"] > 0

    def test_explicit_timeout_overrides_verb_default(self, tmp_path):
        """x-97d8: a caller-supplied timeout_s wins over the per-verb default."""
        from fno.pr_watch._dispatch import fire_skill

        captured = {}

        def stub_runner(cmd, **kw):
            captured.update(kw)
            return _claude_ok_response()

        fire_skill("check", 1, tmp_path, runner=stub_runner, timeout_s=12.0)
        assert captured["timeout"] == 12.0

    def test_runner_timeout_is_failure(self, tmp_path):
        """x-97d8: a real TimeoutExpired now reaches the (previously dead)
        handler and yields a clean failure, not a forever-block."""
        from fno.pr_watch._dispatch import fire_skill

        def stub_runner(cmd, **kw):
            raise subprocess.TimeoutExpired(cmd=cmd, timeout=kw.get("timeout", 0))

        result = fire_skill("check", 1, tmp_path, runner=stub_runner)
        assert result.ok is False
        assert result.is_error is True


# ---------------------------------------------------------------------------
# tick() orchestrator tests
# ---------------------------------------------------------------------------


def _make_tick_deps(
    tmp_path: Path,
    candidates=None,
    obs_map: Optional[dict] = None,
    fire_ok: bool = True,
    claim_held: bool = False,
    node_claimed: bool = False,
    merge_ready: bool = True,
):
    """Build the full set of injectable stubs for tick()."""
    from fno.pr_watch._discover import PrObservation

    events_emitted: list[dict] = []
    notifications: list[dict] = []
    fired: list[dict] = []

    if candidates is None:
        candidates = []

    # graph returns flat list of node dicts that discover_open_prs consumes
    # We pass candidates directly via a pre-built list to avoid graph reading
    cands_list = candidates

    def fake_read_graph(path):
        # Returns the raw entries; discover_open_prs is the consumer
        return []  # tick will use pre_candidates injection

    def fake_discover(entries):
        return cands_list

    # obs_map: pr_number -> PrObservation
    def fake_read_pr_state(candidate, *, reviewers, runner=None, timeout_s=30.0):
        if obs_map and candidate.pr_number in obs_map:
            return obs_map[candidate.pr_number]
        return PrObservation(
            pr_number=candidate.pr_number,
            state="OPEN",
            latest_review_ts=None,
            opened_at="2026-06-01T00:00:00Z",
        )

    def fake_fire_skill(verb, pr_number, repo_dir, *, runner=None, model=None, env_seam=None):
        from fno.pr_watch._dispatch import DispatchResult

        fired.append({"verb": verb, "pr": pr_number, "model": model})
        if fire_ok:
            return DispatchResult(ok=True, rc=0, is_error=False, raw='{"is_error":false}')
        else:
            return DispatchResult(ok=False, rc=0, is_error=True, raw='{"is_error":true}')

    def fake_emit(event_type: str, data: dict):
        events_emitted.append({"type": event_type, "data": data})

    def fake_reviewers_for(repo_dir):
        return ["gemini-code-assist"]

    def fake_post_merge_readiness(repo_root):
        class V:
            is_ready = merge_ready

        return V()

    class FakeClaim:
        """Stub for the claim helper passed to tick()."""

        def __init__(self, held=False, node_live=False):
            self._held = held
            self._node_live = node_live
            self._held_keys: set = set()

        def acquire_tick_lock(self, key, holder):
            if self._held:
                raise _ClaimHeldByOtherStub()
            self._held_keys.add(key)

        def release_tick_lock(self, key, holder):
            self._held_keys.discard(key)

        def acquire_pr_lock(self, key, holder):
            self._held_keys.add(key)

        def release_pr_lock(self, key, holder):
            self._held_keys.discard(key)

        def is_node_live(self, node_id: str) -> bool:
            return self._node_live

    def fake_notify(msg: str, **kw):
        notifications.append({"msg": msg, **kw})

    fake_claim = FakeClaim(held=claim_held, node_live=node_claimed)

    return {
        "events": events_emitted,
        "fired": fired,
        "notifications": notifications,
        "read_graph": fake_read_graph,
        "discover": fake_discover,
        "read_pr_state": fake_read_pr_state,
        "fire_skill": fake_fire_skill,
        "emit": fake_emit,
        "reviewers_for": fake_reviewers_for,
        "claim": fake_claim,
        "notify": fake_notify,
        "post_merge_readiness": fake_post_merge_readiness,
    }


class _ClaimHeldByOtherStub(Exception):
    def __init__(self, holder: str = "pr-watch:4242", pid: int = 4242):
        super().__init__(f"held by {holder}")
        self.holder = holder
        self.pid = pid


class TestTickOrchestrator:
    """Tests for the impure tick() function."""

    def test_empty_graph_emits_heartbeat(self, tmp_path):
        """AC1-UI: empty candidate list still emits pr_watch_tick heartbeat."""
        from fno.pr_watch._dispatch import tick

        store_path = tmp_path / "state.json"
        deps = _make_tick_deps(tmp_path, candidates=[])
        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        tick_events = [e for e in deps["events"] if e["type"] == "pr_watch_tick"]
        assert len(tick_events) == 1
        assert tick_events[0]["data"]["open_prs"] == 0
        assert tick_events[0]["data"]["acted"] == 0

    def test_tick_lock_held_returns_immediately(self, tmp_path):
        """AC-concurrency: if tick lock held, return without discovering/firing."""
        from fno.pr_watch._dispatch import tick

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        deps = _make_tick_deps(tmp_path, candidates=[candidate], claim_held=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=tmp_path / "state.json",
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        # No events at all when lock held (tick exits immediately)
        assert deps["events"] == []
        assert deps["fired"] == []

    def test_AC6_HP_lock_held_tick_names_the_holder(self, tmp_path):
        """A wedged predecessor must not read as an empty sweep."""
        from fno.pr_watch._dispatch import tick

        deps = _make_tick_deps(tmp_path, candidates=[], claim_held=True)
        result = tick(
            graph_path=tmp_path / "graph.json",
            store_path=tmp_path / "state.json",
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )

        assert result.lock_held is True
        assert "pr-watch:4242" in result.lock_holder
        assert "4242" in result.lock_holder
        assert deps["events"] == []

    def test_AC7_EDGE_lock_held_tick_leaves_state_untouched(self, tmp_path):
        """The stale `Last tick:` stays the corroborating staleness signal.

        Runs the SAME candidate both ways. An empty candidate list never
        writes the store at all, so asserting absence on its own would pass
        with the fix reverted and prove nothing.
        """
        from fno.pr_watch._dispatch import tick

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)

        def run(store_path, *, claim_held):
            deps = _make_tick_deps(
                tmp_path, candidates=[candidate], claim_held=claim_held
            )
            tick(
                graph_path=tmp_path / "graph.json",
                store_path=store_path,
                discover_fn=deps["discover"],
                read_pr_state_fn=deps["read_pr_state"],
                fire_skill_fn=deps["fire_skill"],
                emit=deps["emit"],
                reviewers_for=deps["reviewers_for"],
                claim=deps["claim"],
                notify=deps["notify"],
                post_merge_readiness_fn=deps["post_merge_readiness"],
                now_iso="2026-06-14T12:00:00Z",
            )

        healthy = tmp_path / "healthy.json"
        run(healthy, claim_held=False)
        assert healthy.exists(), (
            "this candidate must write the store on a healthy tick, "
            "otherwise the lock-held assertion below is vacuous"
        )

        held = tmp_path / "held.json"
        run(held, claim_held=True)
        assert not held.exists()

    def test_AC6_FR_healthy_tick_keeps_open_prs_reporting(self, tmp_path):
        """The lock_held state is additive: a real sweep is unchanged."""
        from fno.pr_watch._dispatch import tick

        deps = _make_tick_deps(tmp_path, candidates=[])
        result = tick(
            graph_path=tmp_path / "graph.json",
            store_path=tmp_path / "state.json",
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )

        assert result.lock_held is False
        assert result.lock_holder == ""

    def test_no_checkout_emits_skipped(self, tmp_path):
        """AC-no-checkout: candidate with repo_dir=None -> pr_watch_skipped{reason:no-checkout}."""
        from fno.pr_watch._dispatch import tick

        candidate = _make_candidate(pr_number=1, repo_dir=None)
        deps = _make_tick_deps(tmp_path, candidates=[candidate])

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=tmp_path / "state.json",
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        skipped = [e for e in deps["events"] if e["type"] == "pr_watch_skipped"]
        assert any(e["data"]["reason"] == "no-checkout" for e in skipped)
        assert deps["fired"] == []

    def test_live_node_claim_emits_skipped(self, tmp_path):
        """AC-concurrency: node with live node: claim -> pr_watch_skipped{reason:claimed}."""
        from fno.pr_watch._dispatch import tick

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        deps = _make_tick_deps(tmp_path, candidates=[candidate], node_claimed=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=tmp_path / "state.json",
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        skipped = [e for e in deps["events"] if e["type"] == "pr_watch_skipped"]
        assert any(e["data"]["reason"] == "claimed" for e in skipped)
        assert deps["fired"] == []

    def test_first_seen_baselines_no_fire(self, tmp_path):
        """Baseline discipline: first-seen PR records state without firing."""
        from fno.pr_watch._dispatch import tick

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(pr_number=1, state="OPEN")}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=tmp_path / "state.json",
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        # No fire on first-seen; heartbeat emitted but no dispatched event
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert dispatched == []
        assert deps["fired"] == []
        # State was baselined: entry exists in store
        from fno.pr_watch._state import WatermarkStore
        store = WatermarkStore(path=tmp_path / "state.json")
        entry = store.get("owner/repo#1")
        assert entry is not None
        assert entry["last_seen_state"] == "OPEN"

    def test_review_transition_fires_check(self, tmp_path):
        """AC1-UI: new reviewer activity past watermark -> pr_watch_dispatched{kind:review}."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        # Pre-seed the watermark so the PR is NOT first-seen
        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        store.set("owner/repo#1", {
            "last_review_ts": "2026-06-10T00:00:00Z",
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {
            1: _make_obs(pr_number=1, state="OPEN", latest_review_ts="2026-06-12T00:00:00Z")
        }
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, fire_ok=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert len(dispatched) == 1
        assert dispatched[0]["data"]["kind"] == "review"
        assert dispatched[0]["data"]["pr"] == 1

    def test_three_candidates_one_actionable_heartbeat(self, tmp_path):
        """AC1-UI: 3 candidates, 1 actionable -> 1 dispatched + pr_watch_tick{open_prs:3, acted:1}."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)

        # PR 1: no-change (has watermark, no new review)
        store.set("owner/repo#1", {
            "last_review_ts": "2026-06-12T00:00:00Z",
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })
        # PR 2: no-change
        store.set("owner/repo#2", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })
        # PR 3: has new review past watermark -> actionable
        store.set("owner/repo#3", {
            "last_review_ts": "2026-06-10T00:00:00Z",
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        candidates = [
            _make_candidate(pr_number=1, repo_dir=tmp_path, node_id="x-001"),
            _make_candidate(pr_number=2, repo_dir=tmp_path, node_id="x-002"),
            _make_candidate(pr_number=3, repo_dir=tmp_path, node_id="x-003"),
        ]
        obs_map = {
            1: _make_obs(1, "OPEN", latest_review_ts="2026-06-12T00:00:00Z"),  # same ts, no change
            2: _make_obs(2, "OPEN", latest_review_ts=None),  # no reviews
            3: _make_obs(3, "OPEN", latest_review_ts="2026-06-13T00:00:00Z"),  # NEW review
        }
        deps = _make_tick_deps(tmp_path, candidates=candidates, obs_map=obs_map, fire_ok=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )

        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert len(dispatched) == 1

        tick_events = [e for e in deps["events"] if e["type"] == "pr_watch_tick"]
        assert len(tick_events) == 1
        assert tick_events[0]["data"]["open_prs"] == 3
        assert tick_events[0]["data"]["acted"] == 1

    def test_merge_observed_fires_merged(self, tmp_path):
        """AC2-HP: OPEN watermark + MERGED observation + merge_ready -> fire merged once."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        store.set("owner/repo#1", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        _arm_auto_run(tmp_path)
        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "MERGED", merged=True)}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, merge_ready=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert len(dispatched) == 1
        assert dispatched[0]["data"]["kind"] == "merge"

        # merge_dispatched flag set in store (re-read from disk to see tick's writes)
        from fno.pr_watch._state import WatermarkStore as _WS
        fresh_store = _WS(path=store_path)
        entry = fresh_store.get("owner/repo#1")
        assert entry["merge_dispatched"] is True

    def test_merge_already_dispatched_does_not_refire(self, tmp_path):
        """AC2-UI: merge_dispatched=True in watermark -> no re-fire on second tick."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        store.set("owner/repo#1", {
            "last_review_ts": None,
            "last_seen_state": "MERGED",
            "merge_dispatched": True,  # already dispatched
            "retries": 0,
            "parked": None,
        })

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "MERGED", merged=True)}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, merge_ready=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert dispatched == []
        assert deps["fired"] == []

    def test_merge_fire_fails_no_advance(self, tmp_path, monkeypatch):
        """AC2-ERR / AC5-ERR: the cold verb fails -> merge_dispatched stays False, retry scheduled."""
        import fno.post_merge_route as pmr
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        # Override the autouse success verb so the dispatch returns 'failed'.
        monkeypatch.setattr(
            pmr, "_default_run_ritual_verb",
            lambda pr, cwd: pmr.ColdRitualResult(ok=False, tail="fail"),
        )

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        store.set("owner/repo#1", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        _arm_auto_run(tmp_path)
        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "MERGED", merged=True)}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map,
                               merge_ready=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        # merge_dispatched stays False
        entry = store.get("owner/repo#1")
        assert entry["merge_dispatched"] is False
        # dispatch_failed event emitted
        failed_events = [e for e in deps["events"] if e["type"] == "pr_watch_dispatch_failed"]
        assert len(failed_events) == 1
        assert failed_events[0]["data"]["retries"] == 1

    def test_merge_auto_run_off_parks_not_retries(self, tmp_path):
        """x-7930 / codex P1: a 'disabled' dispatch (auto_run opt-in off) is a
        deliberate no-op, NOT a failure: park 'auto-run-disabled', no retry, no
        failure event, no notify."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        WatermarkStore(path=store_path).set("owner/repo#1", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        # No _arm_auto_run: tmp_path has no config -> auto_run defaults off.
        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "MERGED", merged=True)}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, merge_ready=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        # Fresh store to see the tick's persisted writes (the in-memory instance
        # above caches; test_merge_observed_fires_merged re-reads the same way).
        entry = WatermarkStore(path=store_path).get("owner/repo#1")
        assert entry["parked"] == "auto-run-disabled"
        assert entry["merge_dispatched"] is False
        assert [e for e in deps["events"] if e["type"] == "pr_watch_dispatch_failed"] == []
        skipped = [e for e in deps["events"] if e["type"] == "pr_watch_skipped"]
        assert any(e["data"].get("reason") == "auto-run-disabled" for e in skipped)
        assert deps["notifications"] == []

    def test_retry_exhaustion_parks(self, tmp_path):
        """AC1-FR: 3 consecutive failures -> parked{reason:retries-exhausted} + notify."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        store.set("owner/repo#1", {
            "last_review_ts": "2026-06-10T00:00:00Z",
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 2,  # already failed twice
            "parked": None,
        })

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        # New review activity to trigger a review dispatch
        obs_map = {1: _make_obs(1, "OPEN", latest_review_ts="2026-06-12T00:00:00Z")}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, fire_ok=False)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        parked = [e for e in deps["events"] if e["type"] == "pr_watch_parked"]
        assert len(parked) >= 1
        assert any(e["data"]["reason"] == "retries-exhausted" for e in parked)
        # notify called
        assert len(deps["notifications"]) >= 1
        # store entry reflects parked (re-read from disk to see tick's writes)
        from fno.pr_watch._state import WatermarkStore as _WS
        fresh_store = _WS(path=store_path)
        entry = fresh_store.get("owner/repo#1")
        assert entry["parked"] == "retries-exhausted"

    def test_corrupt_store_baseline_no_mass_fire(self, tmp_path):
        """AC-ERR: corrupt store -> first tick baselines (no fire) not mass-fires."""
        from fno.pr_watch._dispatch import tick

        store_path = tmp_path / "state.json"
        store_path.write_text("NOT VALID JSON {{{")  # corrupt

        candidates = [
            _make_candidate(pr_number=1, repo_dir=tmp_path, node_id="x-001"),
            _make_candidate(pr_number=2, repo_dir=tmp_path, node_id="x-002"),
        ]
        obs_map = {
            1: _make_obs(1, "OPEN", latest_review_ts="2026-06-12T00:00:00Z"),
            2: _make_obs(2, "MERGED", merged=True),
        }
        deps = _make_tick_deps(tmp_path, candidates=candidates, obs_map=obs_map,
                               fire_ok=True, merge_ready=True)

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        # Nothing fired: all PRs are first-seen (store was corrupt -> empty)
        assert deps["fired"] == []
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert dispatched == []

    def test_gc_error_does_not_crash_tick(self, tmp_path):
        """AC-ERR: read_pr_state raises ReconcileError -> tick continues, no crash."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore
        from fno.graph._reconcile import ReconcileError

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        # Pre-seed so it's not first-seen; but read_pr_state will fail
        store.set("owner/repo#1", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        def failing_read_pr_state(candidate, *, reviewers, runner=None, timeout_s=30.0):
            raise ReconcileError("gh timed out")

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        deps = _make_tick_deps(tmp_path, candidates=[candidate])
        # Override the read function
        deps["read_pr_state"] = failing_read_pr_state

        # Should not raise
        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        # heartbeat still emitted
        tick_events = [e for e in deps["events"] if e["type"] == "pr_watch_tick"]
        assert len(tick_events) == 1

    # -------------------------------------------------------------------------
    # P2 #3: config.pr_watch.retries is honored (not hardcoded _MAX_RETRIES)
    # -------------------------------------------------------------------------

    def test_configured_retries_1_parks_after_one_failure(self, tmp_path):
        """AC-P2-retries: max_retries=1 parks the PR after a single dispatch failure.

        Codex P2: _MAX_RETRIES is a hardcoded constant; the configured
        config.pr_watch.retries value must be threaded into tick() via a
        max_retries parameter so operators can tune it.
        """
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        # Pre-seed: retries=0, review activity present to trigger a dispatch
        store.set("owner/repo#1", {
            "last_review_ts": "2026-06-10T00:00:00Z",
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "OPEN", latest_review_ts="2026-06-12T00:00:00Z")}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, fire_ok=False)

        # max_retries=1 means: after 1 failure, park immediately
        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
            max_retries=1,
        )

        parked = [e for e in deps["events"] if e["type"] == "pr_watch_parked"]
        assert len(parked) == 1, f"expected 1 parked event, got: {parked}"
        assert parked[0]["data"]["reason"] == "retries-exhausted"

        # Verify state on disk
        fresh = WatermarkStore(path=store_path)
        entry = fresh.get("owner/repo#1")
        assert entry["parked"] == "retries-exhausted"

    # -------------------------------------------------------------------------
    # P1 (P1-dispatch-inert): noop default is never wired in production tick()
    # -------------------------------------------------------------------------

    def test_noop_read_state_never_fires_dispatch(self, tmp_path, monkeypatch):
        """AC-P1-REGRESS: if _noop_read_state is used, dispatch NEVER fires.

        This test documents the regression that the P1 fix must prevent:
        when tick() defaults to _noop_read_state, every PR stays OPEN with
        no review activity and the watcher never fires anything.  After the
        fix, calling tick() without injecting read_pr_state_fn should use the
        REAL read_pr_state, not the noop.

        We verify the invariant from the other direction: the noop path
        (explicitly injected) must NOT fire -- confirming that any live
        firing observed in the real watcher is driven by the real adapter.
        """
        from fno.pr_watch._dispatch import _noop_read_state, tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        # Pre-seed: a MERGED PR with merge_dispatched=False
        store.set("owner/repo#1", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        deps = _make_tick_deps(tmp_path, candidates=[candidate])

        # Explicitly inject the noop: should never fire
        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=_noop_read_state,  # noop: always returns OPEN/no-review
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )

        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert dispatched == [], (
            "noop read_state should never trigger a dispatch; "
            f"got: {dispatched}"
        )
        assert deps["fired"] == []


# ---------------------------------------------------------------------------
# New tests for json.loads guards (gemini HIGH findings)
# ---------------------------------------------------------------------------


class TestJsonLoadsGuards:
    """AC-gemini-HIGH: json.loads returning None/non-dict must not AttributeError."""

    def test_fire_skill_null_json_envelope_is_failure(self, tmp_path):
        """AC-gemini-HIGH _dispatch.py:145: json.loads('null') -> ok=False, no AttributeError."""
        from fno.pr_watch._dispatch import fire_skill

        def stub_runner(cmd, **kw):
            # json.loads('null') returns Python None
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="null", stderr=""
            )

        result = fire_skill("check", 1, tmp_path, runner=stub_runner)
        assert result.ok is False
        assert result.is_error is True

    def test_fire_skill_list_json_envelope_is_failure(self, tmp_path):
        """AC-gemini-HIGH _dispatch.py:145: json.loads('[1,2]') -> ok=False, no AttributeError."""
        from fno.pr_watch._dispatch import fire_skill

        def stub_runner(cmd, **kw):
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="[1, 2, 3]", stderr=""
            )

        result = fire_skill("check", 1, tmp_path, runner=stub_runner)
        assert result.ok is False
        assert result.is_error is True

    def test_watermark_store_null_root_resets_to_empty(self, tmp_path):
        """AC-gemini-HIGH _state.py:148: JSON root 'null' -> store resets to {}, no AttributeError."""
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "state.json"
        path.write_text("null")
        store = WatermarkStore(path=path)
        result = store.load()
        assert result == {}, f"expected empty dict after null root, got: {result!r}"

    def test_watermark_store_list_root_resets_to_empty(self, tmp_path):
        """AC-gemini-HIGH _state.py:148: JSON root '[...]' -> store resets to {}, no AttributeError."""
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "state.json"
        path.write_text('["a", "b"]')
        store = WatermarkStore(path=path)
        result = store.load()
        assert result == {}


class TestReadPrStateJsonGuards:
    """AC-gemini-HIGH _discover.py:240 + medium :243: gh returns null/non-dict JSON."""

    def _make_cand(self, tmp_path):
        from fno.pr_watch._discover import PrCandidate
        return PrCandidate(
            node_id="x-abc",
            pr_number=1,
            pr_url="https://github.com/owner/repo/pull/1",
            repo_dir=tmp_path,
            repo_slug="owner/repo",
        )

    def _merged_state_runner(self, merge_rc=0, merge_stdout=None, view_rc=0, view_stdout=None):
        """Build a runner that handles both query_pr_merge_state calls and gh pr view."""
        call_count = [0]

        def runner(cmd, **kw):
            call_count[0] += 1
            cmd_str = " ".join(str(c) for c in cmd)
            if "mergedAt" in cmd_str:
                # gh pr view call (reviews+createdAt)
                return subprocess.CompletedProcess(
                    args=cmd, returncode=view_rc,
                    stdout=view_stdout or '{"reviews":[],"createdAt":"2026-06-01T00:00:00Z","number":1,"state":"OPEN","url":"","mergedAt":null}',
                    stderr=""
                )
            # merge state call (query_pr_merge_state)
            return subprocess.CompletedProcess(
                args=cmd, returncode=merge_rc,
                stdout=merge_stdout or '{"state":"OPEN","number":1,"url":"https://github.com/owner/repo/pull/1","mergedAt":null}',
                stderr=""
            )

        return runner

    def test_null_gh_view_response_raises_reconcile_error(self, tmp_path):
        """AC-gemini-HIGH _discover:240: gh pr view (reviews+comments) returns 'null' -> ReconcileError, no AttributeError."""
        from fno.graph._reconcile import ReconcileError
        from fno.pr_watch._discover import read_pr_state

        cand = self._make_cand(tmp_path)

        call_count = [0]

        def runner(cmd, **kw):
            call_count[0] += 1
            cmd_str = " ".join(str(c) for c in cmd)
            if "comments" in cmd_str:
                # This is the second gh pr view with reviews+comments+mergedAt -> return null
                return subprocess.CompletedProcess(args=cmd, returncode=0, stdout="null", stderr="")
            # First call: merge state check (query_pr_merge_state) - must succeed
            return subprocess.CompletedProcess(
                args=cmd, returncode=0,
                stdout='{"state":"OPEN","number":1,"url":"https://github.com/owner/repo/pull/1","mergedAt":null}',
                stderr=""
            )

        with pytest.raises(ReconcileError):
            read_pr_state(cand, reviewers=[], runner=runner)

    def test_non_list_reviews_field_is_safe(self, tmp_path):
        """AC-gemini-medium _discover:243: reviews field is not a list -> treated as []."""
        from fno.pr_watch._discover import read_pr_state

        cand = self._make_cand(tmp_path)

        def runner(cmd, **kw):
            cmd_str = " ".join(str(c) for c in cmd)
            if "mergedAt" in cmd_str:
                # reviews is a string instead of a list
                return subprocess.CompletedProcess(
                    args=cmd, returncode=0,
                    stdout='{"reviews":"unexpected-string","createdAt":"2026-06-01T00:00:00Z","number":1,"state":"OPEN","url":"","mergedAt":null}',
                    stderr=""
                )
            return subprocess.CompletedProcess(
                args=cmd, returncode=0,
                stdout='{"state":"OPEN","number":1,"url":"https://github.com/owner/repo/pull/1","mergedAt":null}',
                stderr=""
            )

        # Should not raise; latest_review_ts should be None
        obs = read_pr_state(cand, reviewers=["some-bot"], runner=runner)
        assert obs.latest_review_ts is None


class TestDispatchEntryGuards:
    """AC-gemini-medium _dispatch.py:352 and :410: corrupt store entry + retry increment."""

    def test_non_dict_entry_is_treated_as_none_re_baselines(self, tmp_path):
        """AC-gemini-medium _dispatch:352: a non-dict store entry -> treated as None (re-baseline)."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        # Write a corrupt entry: value is a string, not a dict
        store_path.write_text(json.dumps({"owner/repo#1": "corrupted-string"}))

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "OPEN", latest_review_ts="2026-06-12T00:00:00Z")}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, fire_ok=True)

        # Should not raise AttributeError
        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        # Corrupt entry treated as None -> re-baselined (no dispatch)
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert dispatched == [], (
            "corrupt entry should re-baseline (no dispatch), "
            f"got: {dispatched}"
        )

    def test_null_retries_increments_safely(self, tmp_path):
        """AC-gemini-medium _dispatch:410: retries=null in store -> safely becomes 1 on failure."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        # Retries stored as None (null in JSON) - should safely increment to 1
        store.set("owner/repo#1", {
            "last_review_ts": "2026-06-10T00:00:00Z",
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": None,  # null in JSON
            "parked": None,
        })

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "OPEN", latest_review_ts="2026-06-12T00:00:00Z")}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, fire_ok=False)

        # Should not raise TypeError
        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )

        failed = [e for e in deps["events"] if e["type"] == "pr_watch_dispatch_failed"]
        assert len(failed) == 1
        assert failed[0]["data"]["retries"] == 1


class TestInstallParkedPrsGuards:
    """AC-gemini-medium _install.py:367 and :394: dict-guard parsed events/state."""

    def test_parked_prs_non_dict_entry_skipped(self, tmp_path):
        """AC-gemini-medium _install:394: non-dict entry in state JSON -> skipped, no AttributeError."""
        from fno.pr_watch._install import _parked_prs

        state_path = tmp_path / "state.json"
        # Mix: one valid dict entry, one corrupted non-dict entry
        state_path.write_text(json.dumps({
            "owner/repo#1": {"parked": "retries-exhausted", "retries": 3},
            "owner/repo#2": "corrupted-non-dict",
            "owner/repo#3": None,
        }))

        result = _parked_prs(state_path)

        # Only the valid dict entry with parked set should appear
        assert "owner/repo#1" in result
        assert result["owner/repo#1"] == "retries-exhausted"
        # Non-dict entries must not appear
        assert "owner/repo#2" not in result
        assert "owner/repo#3" not in result

    def test_last_tick_ts_non_dict_event_line_skipped(self, tmp_path):
        """AC-gemini-medium _install:367: non-dict JSON line in events.jsonl -> skipped, no crash."""
        from fno.pr_watch._install import _last_tick_ts

        events_path = tmp_path / "events.jsonl"
        valid_line = json.dumps({"type": "pr_watch_tick", "ts": "2026-06-14T03:00:00Z"})
        # Mix valid line with non-dict lines (null, array, bare string)
        events_path.write_text("\n".join([
            "null",
            "[1, 2, 3]",
            valid_line,
            '"a string"',
        ]))

        result = _last_tick_ts(events_path)
        assert result == "2026-06-14T03:00:00Z"


# ---------------------------------------------------------------------------
# P2 #4 and #5: comments in activity, per-repo reviewers
# ---------------------------------------------------------------------------


class TestCommentsInActivity:
    """AC-P2-comments: bot COMMENT (not formal review) triggers review dispatch."""

    def test_comment_newer_than_watermark_triggers_review_dispatch(self, tmp_path):
        """AC-P2-comments: a bot comment newer than watermark fires /pr check.

        Codex P2 _discover.py:212: read_pr_state only inspects 'reviews';
        comments from configured reviewers must also be included.
        After the fix, latest_review_ts reflects the comment timestamp.
        """
        import subprocess
        from fno.pr_watch._discover import read_pr_state, PrCandidate

        cand = PrCandidate(
            node_id="x-abc",
            pr_number=1,
            pr_url="https://github.com/owner/repo/pull/1",
            repo_dir=tmp_path,
            repo_slug="owner/repo",
        )

        # gh pr view returns: no formal reviews, but a comment from codex-bot
        def runner(cmd, **kw):
            cmd_str = " ".join(str(c) for c in cmd)
            if "mergedAt" in cmd_str or "comments" in cmd_str:
                return subprocess.CompletedProcess(
                    args=cmd, returncode=0,
                    stdout=json.dumps({
                        "reviews": [],
                        "comments": [
                            {
                                "author": {"login": "chatgpt-codex-connector[bot]"},
                                "createdAt": "2026-06-14T02:00:00Z",
                                "body": "P1: fix this",
                            }
                        ],
                        "createdAt": "2026-06-01T00:00:00Z",
                        "number": 1,
                        "state": "OPEN",
                        "url": "https://github.com/owner/repo/pull/1",
                        "mergedAt": None,
                    }),
                    stderr=""
                )
            # merge state
            return subprocess.CompletedProcess(
                args=cmd, returncode=0,
                stdout='{"state":"OPEN","number":1,"url":"https://github.com/owner/repo/pull/1","mergedAt":null}',
                stderr=""
            )

        obs = read_pr_state(cand, reviewers=["codex"], runner=runner)
        # Comment timestamp from codex-bot should be reflected in latest_review_ts
        assert obs.latest_review_ts == "2026-06-14T02:00:00Z", (
            f"expected comment ts in latest_review_ts, got: {obs.latest_review_ts!r}"
        )


class TestPerRepoReviewers:
    """AC-P2-reviewers: _reviewers_for loads config from the candidate repo_dir."""

    def test_reviewers_for_loads_per_repo_config(self, tmp_path):
        """AC-P2-reviewers: two repos with different reviewer config resolve independently.

        Codex P2 cli.py:107: _reviewers_for ignores repo_dir and loads the
        global settings. After the fix, it uses repo_dir to load the per-repo
        settings so each PR uses its own configured reviewers.
        """
        from fno.pr_watch.cli import _reviewers_for

        repo_a = tmp_path / "repo_a"
        repo_b = tmp_path / "repo_b"
        repo_a.mkdir()
        repo_b.mkdir()

        def fake_load_settings_a():
            s = MagicMock()
            s.review.github_apps = ["codex"]
            return s

        def fake_load_settings_b():
            s = MagicMock()
            s.review.github_apps = ["gemini"]
            return s

        # We need to verify _reviewers_for calls load_settings with the repo_dir
        # After the fix, _reviewers_for must pass repo_dir to load_settings.
        # We test that the returned reviewers differ per repo_dir.
        call_log = []

        def fake_load_settings(repo_root=None):
            call_log.append(repo_root)
            if repo_root == repo_a:
                s = MagicMock()
                s.review.github_apps = ["codex"]
                return s
            elif repo_root == repo_b:
                s = MagicMock()
                s.review.github_apps = ["gemini"]
                return s
            else:
                s = MagicMock()
                s.review.github_apps = []
                return s

        with patch("fno.pr_watch.cli.load_settings_for_repo", fake_load_settings):
            result_a = _reviewers_for(repo_a)
            result_b = _reviewers_for(repo_b)

        assert result_a == ["codex"], f"repo_a should get ['codex'], got: {result_a}"
        assert result_b == ["gemini"], f"repo_b should get ['gemini'], got: {result_b}"
        # Verify that load_settings_for_repo was called with the repo dirs
        assert repo_a in call_log, f"load_settings_for_repo not called with repo_a; calls: {call_log}"
        assert repo_b in call_log, f"load_settings_for_repo not called with repo_b; calls: {call_log}"


# ---------------------------------------------------------------------------
# Warm-route merge dispatch (shared marker with reconcile)
# ---------------------------------------------------------------------------


class TestWarmMergeRouting:
    """The tick's merge branch routes through the shared post-merge dispatcher."""

    def _run_merge_tick(self, tmp_path, ritual_outcome, ritual_detail=None):
        from fno.post_merge_route import PostMergeDispatchResult
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        store.set("owner/repo#1", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        candidate = _make_candidate(pr_number=1, repo_dir=tmp_path)
        obs_map = {1: _make_obs(1, "MERGED", merged=True)}
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map, merge_ready=True)

        ritual_calls: list[tuple] = []

        def fake_ritual(cand, obs, fire):
            ritual_calls.append((cand.pr_number, getattr(obs, "merge_sha", None)))
            return PostMergeDispatchResult(
                ritual_outcome, cand.pr_number, short_id="abcd1234", detail=ritual_detail
            )

        tick(
            graph_path=tmp_path / "graph.json",
            store_path=store_path,
            discover_fn=deps["discover"],
            read_pr_state_fn=deps["read_pr_state"],
            fire_skill_fn=deps["fire_skill"],
            dispatch_ritual_fn=fake_ritual,
            emit=deps["emit"],
            reviewers_for=deps["reviewers_for"],
            claim=deps["claim"],
            notify=deps["notify"],
            post_merge_readiness_fn=deps["post_merge_readiness"],
            now_iso="2026-06-14T12:00:00Z",
        )
        return deps, store_path, ritual_calls

    def test_routed_warm_counts_as_dispatched(self, tmp_path):
        """A warm inject is a completed hand-off: watermark advances, no
        headless fire, and the event carries route=warm."""
        from fno.pr_watch._state import WatermarkStore

        deps, store_path, ritual_calls = self._run_merge_tick(tmp_path, "routed-warm")
        assert ritual_calls == [(1, None)]
        assert deps["fired"] == []  # the ritual seam owns any cold fire
        dispatched = [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"]
        assert len(dispatched) == 1
        assert dispatched[0]["data"]["kind"] == "merge"
        assert dispatched[0]["data"]["route"] == "warm"
        entry = WatermarkStore(path=store_path).get("owner/repo#1")
        assert entry["merge_dispatched"] is True

    def test_already_dispatched_marker_exists_advances_watermark(self, tmp_path):
        """US3: reconcile got there first and WROTE the marker (completed dedup)
        -> the daemon marks its watermark and fires nothing."""
        from fno.pr_watch._state import WatermarkStore

        deps, store_path, _calls = self._run_merge_tick(
            tmp_path, "already-dispatched", ritual_detail="marker-exists"
        )
        assert deps["fired"] == []
        assert [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"] == []
        skips = [e for e in deps["events"] if e["type"] == "pr_watch_skipped"]
        assert skips and skips[0]["data"]["reason"] == "already-dispatched"
        entry = WatermarkStore(path=store_path).get("owner/repo#1")
        assert entry["merge_dispatched"] is True

    def test_ritual_claim_live_advances_watermark(self, tmp_path):
        """US4 (x-616b): the guard saw a LIVE reconcile:pr-<n> claim - the ritual
        is already executing (the reconcile that invoked us, or an attended run).
        Detail is ritual-claim-live, not lock-contention, so the daemon advances
        its watermark and fires nothing, exactly like marker-exists."""
        from fno.pr_watch._state import WatermarkStore

        deps, store_path, _calls = self._run_merge_tick(
            tmp_path, "already-dispatched", ritual_detail="ritual-claim-live"
        )
        assert deps["fired"] == []
        assert [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"] == []
        skips = [e for e in deps["events"] if e["type"] == "pr_watch_skipped"]
        assert skips and skips[0]["data"]["reason"] == "already-dispatched"
        entry = WatermarkStore(path=store_path).get("owner/repo#1")
        assert entry["merge_dispatched"] is True  # advanced -> next tick stops re-deciding

    def test_lock_contention_does_not_advance_watermark(self, tmp_path):
        """A concurrent holder is in-flight, NOT done: the daemon must NOT advance
        its watermark, so the next tick retries if that holder later fails before
        writing the marker (else the ritual is silently dropped). The guard's
        SUSPECT path (crashed attended ritual) emits this same detail (x-616b)."""
        from fno.pr_watch._state import WatermarkStore

        deps, store_path, _calls = self._run_merge_tick(
            tmp_path, "already-dispatched", ritual_detail="lock-contention"
        )
        assert deps["fired"] == []
        assert [e for e in deps["events"] if e["type"] == "pr_watch_dispatched"] == []
        skips = [e for e in deps["events"] if e["type"] == "pr_watch_skipped"]
        assert skips and skips[0]["data"]["reason"] == "dispatch-in-flight"
        entry = WatermarkStore(path=store_path).get("owner/repo#1")
        assert entry["merge_dispatched"] is False  # unadvanced -> next tick retries

    def test_spawn_failed_takes_retry_path(self, tmp_path):
        """A failed hand-off leaves the watermark unadvanced and bumps retries."""
        from fno.pr_watch._state import WatermarkStore

        deps, store_path, _calls = self._run_merge_tick(tmp_path, "spawn-failed")
        entry = WatermarkStore(path=store_path).get("owner/repo#1")
        assert entry["merge_dispatched"] is False
        assert entry["retries"] == 1
        failed = [e for e in deps["events"] if e["type"] == "pr_watch_dispatch_failed"]
        assert len(failed) == 1
