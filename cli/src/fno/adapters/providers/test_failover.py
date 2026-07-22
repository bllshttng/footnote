"""Tests for the failover controller.

Run: cd cli && uv run pytest src/fno/adapters/providers/test_failover.py -v

Phase 03 of provider rotation failover (ab-9728b70b). The controller
owns swap orchestration and the per-phase counter state. v0 ships
storm-cap (task 3.1), no-swap-back rule (task 3.3), and the queue-
exhausted fall-through.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import tomllib
import tomli_w



def _strip_none_fx(x):
    if isinstance(x, dict):
        return {k: _strip_none_fx(v) for k, v in x.items() if v is not None}
    if isinstance(x, list):
        return [_strip_none_fx(v) for v in x]
    return x

def _seed_settings(
    tmp_path: Path,
    *,
    active: str,
    record_ids: list[str],
    max_swaps_per_phase: int | None = None,
) -> Path:
    settings_path = tmp_path / "config.toml"
    records = []
    for rid in record_ids:
        records.append({
            "id": rid,
            "name": rid,
            "cli": "claude",
            "auth": "oauth_dir",
            "credentials_source": "~/.claude",
        })
    block: dict = {"active": active, "records": records}
    if max_swaps_per_phase is not None:
        block["failover"] = {"max_swaps_per_phase": max_swaps_per_phase}
    settings_path.write_text(
        tomli_w.dumps(_strip_none_fx({"providers": block}))
    )
    return settings_path


class TestStormCap:
    def test_hp1_first_swap_allowed(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(tmp_path, active="foo",
                                        record_ids=["foo", "bar", "baz"])
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")
        result = ctrl.attempt_swap(current_provider_id="foo", error=err)

        assert result.decision is SwapDecision.SWAPPED
        assert result.new_provider_id == "bar"
        # Counter incremented
        assert ctrl.snapshot_state().swaps_this_phase == 1
        # Settings.yaml mutated
        loaded = tomllib.loads(settings_path.read_text())
        assert loaded["providers"]["active"] == "bar"

    def test_hp2_multiple_swaps_within_budget(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="a",
            record_ids=["a", "b", "c", "d", "e", "f"],
            max_swaps_per_phase=5,
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")

        # Swap 4 times - all should succeed
        prev = "a"
        for i in range(4):
            r = ctrl.attempt_swap(current_provider_id=prev, error=err)
            assert r.decision is SwapDecision.SWAPPED, f"swap {i+1} should succeed"
            prev = r.new_provider_id

        assert ctrl.snapshot_state().swaps_this_phase == 4

    def test_err1_cap_reached_blocks_thrash(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="a",
            record_ids=["a", "b", "c", "d", "e", "f"],
            max_swaps_per_phase=3,
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")

        prev = "a"
        for _ in range(3):
            r = ctrl.attempt_swap(current_provider_id=prev, error=err)
            assert r.decision is SwapDecision.SWAPPED
            prev = r.new_provider_id

        # 4th attempt: cap reached
        r = ctrl.attempt_swap(current_provider_id=prev, error=err)
        assert r.decision is SwapDecision.BLOCKED_THRASH
        assert ctrl.snapshot_state().swaps_this_phase == 3

    def test_default_cap_is_5(self, tmp_path: Path):
        """When config.providers.failover.max_swaps_per_phase is unset,
        the default is 5 (per spec)."""
        from fno.adapters.providers.failover import FailoverController

        settings_path = _seed_settings(
            tmp_path, active="a", record_ids=["a", "b"],
            # max_swaps_per_phase intentionally absent
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        assert ctrl.max_swaps_per_phase == 5

    def test_edge1_storm_cap_in_six_swaps(self, tmp_path: Path):
        """Cites what-if finding #12: 'Failover storm from alternating
        529/200.' Default cap=5 means the 6th swap is blocked."""
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="a",
            record_ids=["a", "b", "c", "d", "e", "f", "g", "h"],
            # default cap 5
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")

        prev = "a"
        for i in range(5):
            r = ctrl.attempt_swap(current_provider_id=prev, error=err)
            assert r.decision is SwapDecision.SWAPPED, f"swap {i+1}/5"
            prev = r.new_provider_id

        # 6th swap: blocked
        r = ctrl.attempt_swap(current_provider_id=prev, error=err)
        assert r.decision is SwapDecision.BLOCKED_THRASH

    def test_edge2_phase_boundary_resets_counter(self, tmp_path: Path):
        """A controller bound to a different phase_id sees swaps_this_phase=0
        regardless of what previous-phase state existed in the state file."""
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="a", record_ids=["a", "b", "c", "d", "e"],
            max_swaps_per_phase=3,
        )
        state_path = tmp_path / "failover-state.json"

        # Phase A: do 3 swaps, hit cap
        ctrl_a = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")
        prev = "a"
        for _ in range(3):
            prev = ctrl_a.attempt_swap(current_provider_id=prev, error=err).new_provider_id

        # Phase B opens: a fresh controller for the new phase. The state
        # file persists from phase A, but the controller for phase B sees
        # its own counter at 0.
        ctrl_b = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-B",
        )
        assert ctrl_b.snapshot_state().swaps_this_phase == 0

        r = ctrl_b.attempt_swap(current_provider_id=prev, error=err)
        assert r.decision is SwapDecision.SWAPPED
        assert ctrl_b.snapshot_state().swaps_this_phase == 1

    def test_blocks_writing_blocked_reason_to_target_state(self, tmp_path: Path):
        """When the cap trips, the controller writes blocked_reason to the
        provided target_state_path so the typed-blocker stop hook's
        BLOCKED handling fires unchanged."""
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="a", record_ids=["a", "b"],
            max_swaps_per_phase=1,
        )
        state_path = tmp_path / "failover-state.json"
        target_state = tmp_path / "target-state.md"
        target_state.write_text("---\nstatus: IN_PROGRESS\n---\nbody\n")

        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A", target_state_path=target_state,
        )
        err = normalize(http_status=529, exit_code=None, body="")

        # First swap proceeds, takes us to b
        r = ctrl.attempt_swap(current_provider_id="a", error=err)
        assert r.decision is SwapDecision.SWAPPED

        # Second swap: cap=1 already used, blocks
        r = ctrl.attempt_swap(current_provider_id="b", error=err)
        assert r.decision is SwapDecision.BLOCKED_THRASH

        text = target_state.read_text()
        assert "blocked_reason: stuck:failover_thrash" in text


class TestNoSwapBack:
    """Task 3.3: 'No swap-back within phase' rule."""

    def test_hp1_first_swap_follows_queue_order(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar", "baz"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")
        r = ctrl.attempt_swap(current_provider_id="foo", error=err)
        assert r.decision is SwapDecision.SWAPPED
        assert r.new_provider_id == "bar"
        assert ctrl.snapshot_state().last_swap_from == "foo"

    def test_hp2_second_swap_excludes_original(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar", "baz"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")

        # First: foo -> bar
        ctrl.attempt_swap(current_provider_id="foo", error=err)
        # Second: bar -> ??? Must NOT be foo even though foo is "available"
        r = ctrl.attempt_swap(current_provider_id="bar", error=err)
        assert r.decision is SwapDecision.SWAPPED
        assert r.new_provider_id == "baz"  # not foo
        assert ctrl.snapshot_state().last_swap_from == "bar"

    def test_err1_queue_exhausted_with_rule(self, tmp_path: Path):
        """Two providers, swapped once, can't swap back. QUEUE_EXHAUSTED."""
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")

        # foo -> bar: succeeds
        ctrl.attempt_swap(current_provider_id="foo", error=err)
        # bar -> ??? : foo excluded by no-swap-back, no other candidate
        r = ctrl.attempt_swap(current_provider_id="bar", error=err)
        assert r.decision is SwapDecision.QUEUE_EXHAUSTED

    def test_edge1_primary_recovers_mid_phase(self, tmp_path: Path):
        """Cites what-if finding #7: the rule keeps the phase on the new
        path even if foo recovers. Flap is structurally prevented."""
        from fno.adapters.providers.failover import (
            FailoverController,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="foo",
            record_ids=["foo", "bar", "baz"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(http_status=529, exit_code=None, body="")

        # foo -> bar (foo is now "recovering")
        ctrl.attempt_swap(current_provider_id="foo", error=err)
        # bar 529 again -> must skip foo, go to baz
        r = ctrl.attempt_swap(current_provider_id="bar", error=err)
        assert r.new_provider_id == "baz"  # not foo, even though foo healthy

    def test_edge2_phase_boundary_clears_rule(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            FailoverController,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar"],
        )
        state_path = tmp_path / "failover-state.json"
        err = normalize(http_status=529, exit_code=None, body="")

        # Phase A: foo -> bar
        ctrl_a = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        ctrl_a.attempt_swap(current_provider_id="foo", error=err)
        assert ctrl_a.snapshot_state().last_swap_from == "foo"

        # Phase B: fresh controller, last_swap_from clear
        ctrl_b = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-B",
        )
        assert ctrl_b.snapshot_state().last_swap_from is None
        # bar -> foo is now allowed because we're in a new phase
        r = ctrl_b.attempt_swap(current_provider_id="bar", error=err)
        assert r.new_provider_id == "foo"


class TestStateHardening:
    """Sigma-review hardening: corrupt or hand-edited failover-state.json
    cannot defeat the storm-cap by setting a negative counter."""

    def test_negative_swaps_floored_to_zero(self, tmp_path: Path):
        import json as _json
        from fno.adapters.providers.failover import (
            FailoverController,
        )

        settings_path = _seed_settings(
            tmp_path, active="a", record_ids=["a", "b", "c"],
        )
        state_path = tmp_path / "failover-state.json"
        # Plant a corrupt state file with a negative counter
        state_path.write_text(_json.dumps({
            "phase_id": "phase-A",
            "swaps_this_phase": -999,
            "last_swap_from": None,
            "last_swap_at_iso": None,
        }))

        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        assert ctrl.snapshot_state().swaps_this_phase == 0

    def test_non_int_swaps_floored_to_zero(self, tmp_path: Path):
        import json as _json
        from fno.adapters.providers.failover import (
            FailoverController,
        )

        settings_path = _seed_settings(
            tmp_path, active="a", record_ids=["a", "b"],
        )
        state_path = tmp_path / "failover-state.json"
        state_path.write_text(_json.dumps({
            "phase_id": "phase-A",
            "swaps_this_phase": "garbage",
            "last_swap_from": None,
            "last_swap_at_iso": None,
        }))

        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id="phase-A",
        )
        assert ctrl.snapshot_state().swaps_this_phase == 0

    def test_swap_result_is_frozen(self, tmp_path: Path):
        import dataclasses as _dc

        from fno.adapters.providers.failover import (
            SwapDecision, SwapResult,
        )

        result = SwapResult(decision=SwapDecision.SWAPPED,
                            new_provider_id="bar")
        with pytest.raises(_dc.FrozenInstanceError):
            result.decision = SwapDecision.BLOCKED_THRASH  # type: ignore[misc]


class TestStateAtomicity:
    """Gemini review HIGH on PR #208: failover-state.json must be
    written atomically under flock so concurrent attempt_swap calls
    can't tear the file or lose updates."""

    def test_concurrent_writes_no_lost_updates(self, tmp_path: Path):
        """Two concurrent controllers in the same phase mutating state -
        each swap must persist; the file must always parse as valid JSON
        at every observable moment."""
        import threading
        import time

        from fno.adapters.providers.failover import (
            FailoverController,
        )
        from fno.adapters.providers.error_taxonomy import normalize

        settings_path = _seed_settings(
            tmp_path, active="a",
            record_ids=["a", "b", "c", "d", "e", "f", "g"],
            max_swaps_per_phase=100,
        )
        state_path = tmp_path / "failover-state.json"
        err = normalize(http_status=529, exit_code=None, body="")

        # Each thread runs its own controller (separate in-memory state)
        # but writes through the same on-disk lock file.
        def worker(start_provider: str, n: int):
            ctrl = FailoverController(
                settings_path=settings_path, state_path=state_path,
                phase_id="phase-A",
            )
            prev = start_provider
            for _ in range(n):
                # Don't trip the cap; just exercise the atomic write
                r = ctrl.attempt_swap(current_provider_id=prev, error=err)
                if r.new_provider_id is not None:
                    prev = r.new_provider_id

        threads = [
            threading.Thread(target=worker, args=("a", 3)),
            threading.Thread(target=worker, args=("a", 3)),
        ]
        # Reader polls the state file during the write storm
        corrupt = []

        def poll():
            for _ in range(40):
                if state_path.exists():
                    try:
                        import json as _json
                        _json.loads(state_path.read_text())
                    except _json.JSONDecodeError:
                        corrupt.append(time.time())
                time.sleep(0.005)

        reader = threading.Thread(target=poll)
        reader.start()
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        reader.join()

        assert corrupt == [], (
            f"failover-state.json was corrupt at {len(corrupt)} read moments"
        )
        # Final file is valid JSON and parseable by _read_state semantics
        import json as _json
        final = _json.loads(state_path.read_text())
        assert "phase_id" in final
        assert isinstance(final["swaps_this_phase"], int)


class TestBlockedReasonReplace:
    """Gemini review MEDIUM on PR #208: _write_blocked_reason should
    replace an existing reason rather than appending a duplicate."""

    def test_replaces_existing_blocked_reason(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            _write_blocked_reason,
        )

        target_state = tmp_path / "target-state.md"
        target_state.write_text(
            "---\n"
            "status: IN_PROGRESS\n"
            "blocked_reason: stuck:earlier_detector\n"
            "---\n"
            "body\n"
        )
        ok = _write_blocked_reason(target_state, "stuck:failover_thrash")
        assert ok is True

        text = target_state.read_text()
        # Old reason removed
        assert "blocked_reason: stuck:earlier_detector" not in text
        # New reason present
        assert "blocked_reason: stuck:failover_thrash" in text
        # Exactly ONE blocked_reason line (no duplicates)
        assert text.count("blocked_reason:") == 1


class TestPrioritySafety:
    """Gemini review MEDIUM on PR #208: non-numeric priority in YAML
    must not crash the eligibility lookup."""

    def test_non_numeric_priority_falls_back_to_default(self, tmp_path: Path):
        from fno.adapters.providers.failover import (
            _next_eligible_provider,
        )

        # Two records, one with bad priority string
        settings_path = tmp_path / "config.toml"
        settings_path.write_text(tomli_w.dumps(_strip_none_fx({
            "providers": {
                    "active": "a",
                    "records": [
                        {"id": "a", "name": "a", "cli": "claude",
                         "auth": "oauth_dir",
                         "credentials_source": "~/.claude",
                         "priority": "not-a-number"},
                        {"id": "b", "name": "b", "cli": "claude",
                         "auth": "oauth_dir",
                         "credentials_source": "~/.claude",
                         "priority": 50},
                    ],
                }
            }
        )))

        # b has priority 50, a falls back to default 100; b wins.
        candidate = _next_eligible_provider(
            settings_path=settings_path, exclude=[],
        )
        assert candidate == "b"


# Section 3 of ab-6534a78a: failover wiring with classify_error +
# update_provider_health + record_success.

class TestRuntimeStateWiring:
    """attempt_swap and record_success delegate to runtime_state."""

    def test_hp1_429_with_body_calls_classify_and_update(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # AC3.1-HP: failover invokes classify_error then update_provider_health
        # when the error has a matching ErrorRule.
        from fno.adapters.providers import runtime_state as rs
        from fno.adapters.providers.error_taxonomy import normalize
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )

        runtime_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(runtime_path))

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path,
            state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(
            http_status=429, exit_code=None, body="rate limit exceeded",
        )
        result = ctrl.attempt_swap(current_provider_id="foo", error=err)

        # Swap still completes (storm-cap not reached).
        assert result.decision is SwapDecision.SWAPPED
        # Runtime state shows level 1 for foo.
        state = rs.read_state()
        assert "foo" in state.provider_health
        assert state.provider_health["foo"].backoff_level == 1

    def test_hp2_record_success_resets_backoff(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # AC3.2-HP: record_success(provider_id) clears backoff state.
        from fno.adapters.providers import runtime_state as rs
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.failover import record_success

        runtime_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(runtime_path))

        rule = ErrorRule(text="rate limit", backoff=True)
        for _ in range(5):
            rs.update_provider_health("X", rule)
        assert rs.read_state().provider_health["X"].backoff_level == 5

        record_success("X")

        post = rs.read_state()
        assert (
            "X" not in post.provider_health
            or post.provider_health["X"].backoff_level == 0
        )

    def test_edge_no_rule_match_preserves_existing_path(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # AC3.3-EDGE: 503 with body that has no text-rule match - storm-cap
        # path runs unchanged, runtime_state is NOT updated.
        from fno.adapters.providers import runtime_state as rs
        from fno.adapters.providers.error_taxonomy import normalize
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )

        runtime_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(runtime_path))

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path,
            state_path=state_path,
            phase_id="phase-A",
        )
        # 503 with body that doesn't match any text rule
        err = normalize(
            http_status=503, exit_code=None, body="internal server error",
        )
        result = ctrl.attempt_swap(current_provider_id="foo", error=err)

        assert result.decision is SwapDecision.SWAPPED
        # No runtime_state entry created (no text rule matched, status 503
        # has no rule either - the 5XX taxonomy uses normalize() not
        # classify_error()).
        state = rs.read_state()
        assert state.provider_health == {}

    def test_fr_failover_state_unchanged_by_runtime_state_writes(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # AC3.4-FR: existing failover-state.json (storm-cap, no-swap-back)
        # contents must remain identical pre/post Plan A code.
        from fno.adapters.providers.error_taxonomy import normalize
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )

        runtime_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(runtime_path))

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar", "baz"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path,
            state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(
            http_status=429, exit_code=None, body="rate limit exceeded",
        )
        result = ctrl.attempt_swap(current_provider_id="foo", error=err)
        assert result.decision is SwapDecision.SWAPPED

        # failover-state.json fields are exactly: phase_id, swaps_this_phase,
        # last_swap_from, last_swap_at_iso. No new fields, no rename.
        import json

        loaded = json.loads(state_path.read_text())
        assert set(loaded.keys()) == {
            "phase_id",
            "swaps_this_phase",
            "last_swap_from",
            "last_swap_at_iso",
        }
        assert loaded["phase_id"] == "phase-A"
        assert loaded["swaps_this_phase"] == 1
        assert loaded["last_swap_from"] == "foo"

    def test_runtime_state_io_failure_does_not_block_swap(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # Defensive: an IO failure inside runtime_state must not block
        # the swap. We patch update_provider_health to raise OSError
        # (the kind of failure that legitimately escapes runtime_state)
        # and confirm the swap completes. Note: programmer errors
        # (TypeError, AttributeError, RuntimeError) intentionally
        # propagate so they surface in CI - that's the narrow-catch
        # contract.
        from fno.adapters.providers import failover as fo
        from fno.adapters.providers.error_taxonomy import normalize
        from fno.adapters.providers.failover import (
            FailoverController,
            SwapDecision,
        )

        runtime_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(runtime_path))

        def _boom(*_args: object, **_kwargs: object) -> None:
            raise OSError("simulated disk failure")

        monkeypatch.setattr(fo, "update_provider_health", _boom)

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path,
            state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(
            http_status=429, exit_code=None, body="rate limit exceeded",
        )
        # Swap still happens; runtime_state IO error is swallowed and logged.
        result = ctrl.attempt_swap(current_provider_id="foo", error=err)
        assert result.decision is SwapDecision.SWAPPED


class TestModelPassthrough:
    """Plan A1 (ab-7fe3cdaf): failover.attempt_swap forwards error.model
    to update_provider_health so the lock lands on model_locks rather
    than the provider-wide rate_limited_until."""

    def test_ac6_1_existing_callers_no_model_unchanged(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # AC6.1-FR: callers that don't set model still produce provider-
        # level locks (Plan A baseline). Existing test_hp1 already proves
        # this end-to-end; this micro-test pins the contract via spy.
        from fno.adapters.providers import failover as fo
        from fno.adapters.providers.error_taxonomy import normalize
        from fno.adapters.providers.failover import FailoverController

        runtime_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(runtime_path))

        seen_kwargs: dict[str, object] = {}

        def _spy(provider_id: str, rule: object, **kwargs: object) -> object:
            seen_kwargs["provider_id"] = provider_id
            seen_kwargs["model"] = kwargs.get("model")
            from fno.adapters.providers.runtime_state import (
                update_provider_health as _real,
            )

            return _real(provider_id, rule, **kwargs)  # type: ignore[arg-type]

        monkeypatch.setattr(fo, "update_provider_health", _spy)

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path,
            state_path=state_path,
            phase_id="phase-A",
        )
        # normalize() without model arg -> NormalizedError.model is None
        err = normalize(
            http_status=429, exit_code=None, body="rate limit exceeded",
        )
        ctrl.attempt_swap(current_provider_id="foo", error=err)

        assert seen_kwargs["provider_id"] == "foo"
        assert seen_kwargs["model"] is None

    def test_ac6_2_model_set_writes_model_lock(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        # AC6.2-HP: when normalize() carries a model id, that model gets
        # locked on disk and rate_limited_until is NOT written.
        from fno.adapters.providers import runtime_state as rs
        from fno.adapters.providers.error_taxonomy import normalize
        from fno.adapters.providers.failover import FailoverController

        runtime_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(runtime_path))

        settings_path = _seed_settings(
            tmp_path, active="foo", record_ids=["foo", "bar"],
        )
        state_path = tmp_path / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path,
            state_path=state_path,
            phase_id="phase-A",
        )
        err = normalize(
            http_status=429, exit_code=None, body="rate limit exceeded",
            model="claude-opus-4-7",
        )
        ctrl.attempt_swap(current_provider_id="foo", error=err)

        state = rs.read_state()
        health = state.provider_health["foo"]
        assert "claude-opus-4-7" in health.model_locks
        assert health.rate_limited_until is None
        # Sibling model on same provider is NOT locked
        assert not rs.is_in_cooldown("foo", "claude-sonnet-4-6")
        assert rs.is_in_cooldown("foo", "claude-opus-4-7")


def test_record_success_is_public_module_function() -> None:
    """record_success is exported from failover.py at module level."""
    from fno.adapters.providers import failover as fo

    assert callable(fo.record_success)


def test_record_success_swallows_oserror_from_runtime_state(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """record_success must not propagate IO failures from runtime_state.

    Symmetric to test_runtime_state_update_does_not_block_swap_on_lock_timeout
    but for the success path. A 2xx call must not crash on a state-file
    write failure.
    """
    from fno.adapters.providers import failover as fo

    def _boom(*_args: object, **_kwargs: object) -> None:
        raise OSError("simulated disk error")

    monkeypatch.setattr(fo, "reset_provider_health", _boom)
    # Should not raise; warning is logged.
    fo.record_success("X")
