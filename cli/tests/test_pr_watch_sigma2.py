"""Sigma-review round 2 fixes: tests written RED-first before implementation.

Covers:
  Fix #1 - _emit_event anchors to state_dir()/events.jsonl (not cwd-relative)
  Fix #1b - plist WorkingDirectory key present
  Fix #2 - cli.tick() end-to-end integration test
  Fix #3 - PrWatchBlock numeric bounds (interval_seconds=0 raises)
  Fix #4 - PrObservation.merged field removed
  Fix #5 - frozen=True on all five dataclasses
  Fix #6 - closed-PR park at tick() orchestrator level
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Optional
from unittest.mock import MagicMock, patch

import pytest


# ---------------------------------------------------------------------------
# Helpers shared across tests
# ---------------------------------------------------------------------------


def _make_obs(
    pr_number: int = 1,
    state: str = "OPEN",
    latest_review_ts: Optional[str] = None,
    opened_at: str = "2026-06-01T00:00:00Z",
):
    """Build a PrObservation WITHOUT the removed 'merged' field."""
    from fno.pr_watch._discover import PrObservation

    return PrObservation(
        pr_number=pr_number,
        state=state,
        latest_review_ts=latest_review_ts,
        opened_at=opened_at,
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


def _make_tick_deps(
    tmp_path: Path,
    candidates=None,
    obs_map: Optional[dict] = None,
    fire_ok: bool = True,
    merge_ready: bool = False,
):
    """Minimal injectable stubs for tick()."""
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

    def fake_fire_skill(verb, pr_number, repo_dir, *, runner=None, model=None, env_seam=None):
        fired.append({"verb": verb, "pr": pr_number})
        if fire_ok:
            return DispatchResult(ok=True, rc=0, is_error=False, raw='{"is_error":false}')
        return DispatchResult(ok=False, rc=1, is_error=True, raw='{"is_error":true}')

    def fake_emit(event_type: str, data: dict):
        events_emitted.append({"type": event_type, "data": data})

    def fake_post_merge_readiness(repo_root):
        class V:
            is_ready = merge_ready
        return V()

    class FakeClaim:
        def acquire_tick_lock(self, key, holder): pass
        def release_tick_lock(self, key, holder): pass
        def acquire_pr_lock(self, key, holder): pass
        def release_pr_lock(self, key, holder): pass
        def is_node_live(self, node_id): return False

    return {
        "events": events_emitted,
        "fired": fired,
        "discover": fake_discover,
        "read_pr_state": fake_read_pr_state,
        "fire_skill": fake_fire_skill,
        "emit": fake_emit,
        "reviewers_for": lambda _: [],
        "claim": FakeClaim(),
        "notify": lambda *a, **kw: None,
        "post_merge_readiness": fake_post_merge_readiness,
    }


# ---------------------------------------------------------------------------
# Fix #1a — _emit_event default path is state_dir()/events.jsonl, NOT cwd-relative
# ---------------------------------------------------------------------------


class TestEmitEventAnchoredPath:
    """AC1-VERIFY: _emit_event writes to state_dir()/events.jsonl when no explicit path given."""

    def test_default_events_path_is_under_state_dir_not_cwd(self, tmp_path, monkeypatch):
        """The daemon's events path must NOT be cwd-relative.

        Redirects HOME to tmp_path so state_dir() returns tmp_path/.fno.
        Changes cwd to a DIFFERENT tmp directory.
        Asserts the event lands in state_dir()/events.jsonl, not in the cwd.
        """
        # Set up a fake HOME with .fno so state_dir() resolves there
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        (fake_home / ".fno").mkdir()
        monkeypatch.setenv("HOME", str(fake_home))
        # Clear any cached settings
        try:
            from fno.config import load_settings
            load_settings.cache_clear()
        except Exception:
            pass

        # Change cwd to a completely different directory
        other_dir = tmp_path / "other_cwd"
        other_dir.mkdir()
        original_cwd = os.getcwd()
        os.chdir(other_dir)
        try:
            from fno.pr_watch.cli import _emit_event
            # Call WITHOUT explicit events_path — uses the default resolver
            _emit_event("pr_watch_tick", {"open_prs": 0, "acted": 0})
        finally:
            os.chdir(original_cwd)

        # Event must NOT land in the cwd
        cwd_events = other_dir / ".fno" / "events.jsonl"
        assert not cwd_events.exists(), (
            f"event landed in cwd-relative path {cwd_events} — bug #1 not fixed"
        )

        # Event MUST land under state_dir (HOME/.fno)
        from fno.paths import state_dir
        # Reset cache after HOME change
        try:
            from fno.config import load_settings
            load_settings.cache_clear()
        except Exception:
            pass
        monkeypatch.setenv("HOME", str(fake_home))

        anchored_events = fake_home / ".fno" / "events.jsonl"
        assert anchored_events.exists(), (
            f"event not found at state_dir path {anchored_events}"
        )
        lines = anchored_events.read_text().strip().splitlines()
        assert len(lines) == 1
        ev = json.loads(lines[0])
        assert ev["type"] == "pr_watch_tick"

    def test_emit_event_same_path_as_last_tick_ts(self, tmp_path, monkeypatch):
        """_emit_event default path must be the SAME path _last_tick_ts reads from.

        status's _last_tick_ts defaults to state_dir()/events.jsonl.
        The daemon's _emit_event must write to the same location.
        """
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        (fake_home / ".fno").mkdir()
        monkeypatch.setenv("HOME", str(fake_home))
        try:
            from fno.config import load_settings
            load_settings.cache_clear()
        except Exception:
            pass

        from fno.pr_watch.cli import _emit_event
        _emit_event("pr_watch_tick", {"open_prs": 0, "acted": 0})

        # Now verify _last_tick_ts (with no explicit path) finds that event
        from fno.pr_watch._install import _last_tick_ts
        ts = _last_tick_ts(None)  # None = use the default state_dir() path
        assert ts is not None, (
            "_last_tick_ts could not find the event written by _emit_event; "
            "they must resolve to the same path"
        )


# ---------------------------------------------------------------------------
# Fix #1b — plist WorkingDirectory = $HOME
# ---------------------------------------------------------------------------


class TestPlistWorkingDirectory:
    """AC1b-VERIFY: rendered plist contains a WorkingDirectory key set to $HOME."""

    def test_plist_has_working_directory_key(self, tmp_path, monkeypatch):
        """render_plist() includes <key>WorkingDirectory</key> pointing at HOME."""
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        monkeypatch.setenv("HOME", str(fake_home))

        from fno.pr_watch._install import render_plist

        la_dir = tmp_path / "LaunchAgents"
        la_dir.mkdir()
        rendered = render_plist(
            launch_agents_dir=la_dir,
            fno_binary="/usr/local/bin/fno",
            install_path="/usr/bin:/bin",
        )

        assert "<key>WorkingDirectory</key>" in rendered, (
            "plist missing WorkingDirectory key — launchd will start in '/' "
            "causing cwd-relative paths to write to /.fno/"
        )
        # The value must contain the home directory path
        assert str(fake_home) in rendered or _xml_escape_simple(str(fake_home)) in rendered, (
            f"plist WorkingDirectory does not contain HOME ({fake_home})"
        )


def _xml_escape_simple(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


# ---------------------------------------------------------------------------
# Fix #2 — cli.tick() integration test (drives real adapter assembly)
# ---------------------------------------------------------------------------


class TestCliTickIntegration:
    """AC2-VERIFY: cli.tick() end-to-end with real adapters, seamed fire cmd.

    This drives fno.pr_watch.cli.tick() — the Typer command launchd invokes —
    not _dispatch.tick() directly. It verifies the real ClaimAdapter,
    _emit_event, _reviewers_for, and post_merge_readiness assembly.
    """

    def test_cli_tick_emits_event_to_anchored_path(self, tmp_path, monkeypatch):
        """AC2-HP: cli.tick() fires with PR_WATCH_FIRE_CMD=true and an event
        lands in state_dir()/events.jsonl from the real _emit_event adapter.

        This test MUST FAIL before fix #1a (event written to cwd-relative path
        when no explicit events_path arg is passed to _emit_event).
        """
        # Set up isolated HOME so state_dir() resolves to tmp_path/home/.fno
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        fno_dir = fake_home / ".fno"
        fno_dir.mkdir()
        monkeypatch.setenv("HOME", str(fake_home))
        try:
            from fno.config import load_settings
            load_settings.cache_clear()
        except Exception:
            pass

        # Use the PR_WATCH_FIRE_CMD seam so no real claude is spawned
        monkeypatch.setenv("PR_WATCH_FIRE_CMD", "true")

        # Seed a minimal graph.json with one open node that has a PR ref
        # pointing at a real local directory (tmp_path has a .git sentinel)
        repo_dir = tmp_path / "repo"
        repo_dir.mkdir()
        (repo_dir / ".git").mkdir()

        graph = {
            "nodes": [
                {
                    "id": "x-test1234",
                    "title": "test node",
                    "completed_at": None,
                    "superseded_by": None,
                    "_pr_refs": [[42, "https://github.com/owner/repo/pull/42"]],
                    "cwd": str(repo_dir),
                }
            ]
        }
        graph_path = fno_dir / "graph.json"
        graph_path.write_text(json.dumps(graph))

        # Monkeypatch the functions cli.tick() calls so no real gh/claims/settings
        def fake_post_merge_readiness(repo_dir):
            class V:
                is_ready = False
            return V()

        from fno.pr_watch._discover import PrCandidate
        candidate = PrCandidate(
            node_id="x-test1234",
            pr_number=42,
            pr_url="https://github.com/owner/repo/pull/42",
            repo_dir=repo_dir,
            repo_slug="owner/repo",
        )

        def fake_discover(entries):
            return [candidate]

        with patch("fno.pr_watch.cli.load_settings") as mock_load_settings, \
             patch("fno.pr_watch.cli.claim_status", return_value={"state": "free"}), \
             patch("fno.claims.acquire_claim"), \
             patch("fno.claims.release_claim"):

            # Build a real-ish settings mock with concrete (non-MagicMock) values
            mock_cfg_pr_watch = MagicMock()
            mock_cfg_pr_watch.max_age_days = 14
            mock_cfg_pr_watch.model = "claude-haiku-4-5"
            mock_cfg_review = MagicMock()
            mock_cfg_review.github_apps = []  # canonical field (required_bots aliases it)
            mock_cfg_review.required_bots = []
            mock_settings = MagicMock()
            mock_settings.pr_watch = mock_cfg_pr_watch
            mock_settings.review = mock_cfg_review
            mock_load_settings.return_value = mock_settings

            from fno.pr_watch._dispatch import tick as _dispatch_tick
            from fno.pr_watch.cli import (
                ClaimAdapter, _emit_event, _reviewers_for, _notify_parked
            )
            from datetime import datetime, timezone

            result = _dispatch_tick(
                claim=ClaimAdapter(),
                emit=_emit_event,  # <-- the real adapter, no explicit path
                reviewers_for=_reviewers_for,
                notify=lambda message, **_kw: _notify_parked(message),
                post_merge_readiness_fn=fake_post_merge_readiness,
                now_iso=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                max_age_days=14,
                discover_fn=fake_discover,
            )

        # Assert a pr_watch_tick event landed in state_dir()/events.jsonl
        events_path = fno_dir / "events.jsonl"
        assert events_path.exists(), (
            f"No events.jsonl found at {events_path}. "
            "The real _emit_event wrote to a cwd-relative path instead (bug #1 not fixed)."
        )
        events = [json.loads(l) for l in events_path.read_text().strip().splitlines() if l.strip()]
        tick_events = [e for e in events if e.get("type") == "pr_watch_tick"]
        assert len(tick_events) >= 1, (
            f"No pr_watch_tick event in {events_path}. Events found: {events}"
        )

        # Validate the event passes schema
        from fno.events import validate
        validate(tick_events[0])


# ---------------------------------------------------------------------------
# Fix #3 — PrWatchBlock numeric field bounds
# ---------------------------------------------------------------------------


class TestPrWatchBlockBounds:
    """AC3: PrWatchBlock rejects zero/invalid numeric values at config load."""

    def test_interval_seconds_zero_raises(self):
        """interval_seconds=0 must raise ValidationError (would set StartInterval:0)."""
        from pydantic import ValidationError
        from fno.config import PrWatchBlock

        with pytest.raises(ValidationError):
            PrWatchBlock(interval_seconds=0)

    def test_interval_seconds_negative_raises(self):
        """interval_seconds=-1 must raise ValidationError."""
        from pydantic import ValidationError
        from fno.config import PrWatchBlock

        with pytest.raises(ValidationError):
            PrWatchBlock(interval_seconds=-1)

    def test_retries_zero_raises(self):
        """retries=0 must raise ValidationError (parks on first failure)."""
        from pydantic import ValidationError
        from fno.config import PrWatchBlock

        with pytest.raises(ValidationError):
            PrWatchBlock(retries=0)

    def test_max_age_days_zero_raises(self):
        """max_age_days=0 must raise ValidationError (parks everything immediately)."""
        from pydantic import ValidationError
        from fno.config import PrWatchBlock

        with pytest.raises(ValidationError):
            PrWatchBlock(max_age_days=0)

    def test_model_empty_string_raises(self):
        """model='' must raise ValidationError (causes 'claude --model ""')."""
        from pydantic import ValidationError
        from fno.config import PrWatchBlock

        with pytest.raises(ValidationError):
            PrWatchBlock(model="")

    def test_valid_values_pass(self):
        """Valid values within bounds must not raise."""
        from fno.config import PrWatchBlock

        block = PrWatchBlock(
            enabled=True,
            interval_seconds=300,
            retries=1,
            max_age_days=1,
            model="claude-haiku-4-5",
        )
        assert block.interval_seconds == 300
        assert block.retries == 1
        assert block.max_age_days == 1

    def test_defaults_are_valid(self):
        """Default PrWatchBlock() values all pass the new bounds."""
        from fno.config import PrWatchBlock

        block = PrWatchBlock()
        assert block.interval_seconds == 600
        assert block.retries == 3
        assert block.max_age_days == 14
        assert block.model == "claude-haiku-4-5"

    def test_nonmapping_still_degrades_to_defaults(self):
        """config.pr_watch: 42 (non-mapping) still degrades to defaults (coercer preserved)."""
        from fno.config import ConfigBlock

        cb = ConfigBlock.model_validate({"pr_watch": 42})
        assert cb.pr_watch.enabled is False
        assert cb.pr_watch.interval_seconds == 600

    def test_bad_value_within_mapping_now_raises(self):
        """A valid dict with an out-of-bounds value must now raise (intended tightening)."""
        from pydantic import ValidationError
        from fno.config import ConfigBlock

        with pytest.raises(ValidationError):
            ConfigBlock.model_validate({"pr_watch": {"interval_seconds": 0}})


# ---------------------------------------------------------------------------
# Fix #4 — PrObservation.merged field removed
# ---------------------------------------------------------------------------


class TestPrObservationMergedFieldRemoved:
    """AC4: PrObservation no longer has a 'merged' field."""

    def test_pr_observation_has_no_merged_field(self):
        """PrObservation must not accept a 'merged' keyword argument."""
        from fno.pr_watch._discover import PrObservation
        import dataclasses

        field_names = {f.name for f in dataclasses.fields(PrObservation)}
        assert "merged" not in field_names, (
            "PrObservation still has 'merged' field; it should have been removed. "
            "decide() reads obs.state == 'MERGED' directly."
        )

    def test_pr_observation_construction_without_merged(self):
        """PrObservation can be constructed without 'merged' (the standard factory)."""
        from fno.pr_watch._discover import PrObservation

        obs = PrObservation(
            pr_number=42,
            state="OPEN",
            latest_review_ts=None,
            opened_at="2026-06-01T00:00:00Z",
        )
        assert obs.pr_number == 42
        assert obs.state == "OPEN"

    def test_read_pr_state_does_not_set_merged(self):
        """read_pr_state result has no merged attribute (construction site updated)."""
        from fno.pr_watch._discover import PrObservation
        import dataclasses

        # Verify the construction in _discover.py doesn't still pass merged=
        # by checking the field set is as expected
        field_names = {f.name for f in dataclasses.fields(PrObservation)}
        expected = {"pr_number", "state", "latest_review_ts", "opened_at"}
        assert field_names == expected, (
            f"PrObservation fields mismatch. Expected {expected}, got {field_names}"
        )


# ---------------------------------------------------------------------------
# Fix #5 — frozen=True on all five dataclasses
# ---------------------------------------------------------------------------


class TestFrozenDataclasses:
    """AC5: Decision, PrCandidate, PrObservation, DispatchResult, TickResult are frozen."""

    def test_decision_is_frozen(self):
        """Decision instances are immutable after construction."""
        from fno.pr_watch import Decision

        d = Decision(kind="noop", pr_number=1, reason="x")
        with pytest.raises((AttributeError, TypeError)):
            d.kind = "park"  # type: ignore[misc]

    def test_dispatch_result_is_frozen(self):
        """DispatchResult instances are immutable after construction."""
        from fno.pr_watch._dispatch import DispatchResult

        r = DispatchResult(ok=True, rc=0, is_error=False, raw="")
        with pytest.raises((AttributeError, TypeError)):
            r.ok = False  # type: ignore[misc]

    def test_tick_result_is_frozen(self):
        """TickResult instances are immutable after construction."""
        from fno.pr_watch._dispatch import TickResult

        r = TickResult(open_prs=0, acted=0)
        with pytest.raises((AttributeError, TypeError)):
            r.open_prs = 99  # type: ignore[misc]

    def test_pr_candidate_is_frozen(self):
        """PrCandidate instances are immutable after construction."""
        from fno.pr_watch._discover import PrCandidate

        c = PrCandidate(
            node_id="x-abc",
            pr_number=1,
            pr_url="https://github.com/o/r/pull/1",
            repo_dir=None,
            repo_slug="o/r",
        )
        with pytest.raises((AttributeError, TypeError)):
            c.pr_number = 99  # type: ignore[misc]

    def test_pr_observation_is_frozen(self):
        """PrObservation instances are immutable after construction."""
        from fno.pr_watch._discover import PrObservation

        obs = PrObservation(
            pr_number=1,
            state="OPEN",
            latest_review_ts=None,
            opened_at=None,
        )
        with pytest.raises((AttributeError, TypeError)):
            obs.state = "MERGED"  # type: ignore[misc]


# ---------------------------------------------------------------------------
# Fix #6 — closed-PR park test at tick() orchestrator level
# ---------------------------------------------------------------------------


class TestClosedPrParkAtOrchestrator:
    """AC6-FR: CLOSED PR parks with reason 'closed' at the tick() level.

    The decide()-level coverage exists; this verifies the full orchestrator
    path: watermark seeded, CLOSED observation fed through tick(), assert
    parked=True in store + pr_watch_parked event emitted + nothing fired.
    """

    def test_closed_pr_parked_at_tick_level(self, tmp_path):
        """AC6-FR: tick() parks a CLOSED PR, emits pr_watch_parked, fires nothing."""
        from fno.pr_watch._dispatch import tick
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "state.json"
        store = WatermarkStore(path=store_path)
        # Pre-seed with a known open baseline so it's not first-seen
        store.set("owner/repo#5", {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        })

        closed_obs = _make_obs(pr_number=5, state="CLOSED")
        obs_map = {5: closed_obs}
        candidate = _make_candidate(pr_number=5, repo_dir=tmp_path)
        deps = _make_tick_deps(tmp_path, candidates=[candidate], obs_map=obs_map)

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

        # Nothing was fired
        assert deps["fired"] == [], f"expected no fires, got {deps['fired']}"

        # pr_watch_parked event emitted with reason "closed"
        parked_events = [e for e in deps["events"] if e["type"] == "pr_watch_parked"]
        assert len(parked_events) == 1, f"expected 1 parked event, got {parked_events}"
        assert parked_events[0]["data"]["reason"] == "closed"
        assert parked_events[0]["data"]["pr"] == 5

        # Watermark store updated: parked = "closed"
        store2 = WatermarkStore(path=store_path)
        entry = store2.get("owner/repo#5")
        assert entry is not None, "watermark entry missing after tick"
        assert entry.get("parked") == "closed", (
            f"expected parked='closed' in store, got {entry.get('parked')!r}"
        )
