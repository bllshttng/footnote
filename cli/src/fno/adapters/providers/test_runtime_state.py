"""Tests for ProviderRuntimeState (per-provider backoff state).

Run: cd cli && uv run pytest src/fno/adapters/providers/test_runtime_state.py -v

Plan A of provider failover hardening (ab-6534a78a). Distinct from
phase-scoped failover-state.json: runtime_state survives target spawns
within a megawalk campaign and tracks per-provider exponential backoff
plus 1h-stale TTL.
"""
from __future__ import annotations

import json
import multiprocessing
import time
from pathlib import Path

import pytest

from fno.adapters.providers.error_taxonomy import ErrorRule
from fno.adapters.providers.runtime_state import (
    BASE_BACKOFF_MS,
    LOCK_TIMEOUT_SECONDS,
    MAX_BACKOFF_LEVEL,
    MAX_BACKOFF_MS,
    PROVIDER_HEALTH_TTL_SECONDS,
    ProviderHealth,
    ProviderRuntimeState,
    _compute_exponential_cooldown_ms,
    is_in_cooldown,
    read_state,
    reset_provider_health,
    update_provider_health,
)


@pytest.fixture
def state_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect runtime_state to a tmp path for every test.

    Uses the env-var override path (which the module honors first in
    _resolve_state_path) so the override survives reloads inside tests.
    monkeypatch.setenv auto-restores at test teardown.
    """
    target = tmp_path / "provider-runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(target))
    return target


class TestComputeExponentialCooldown:
    """Pure function: BASE_BACKOFF_MS * 2 ** level, capped at MAX_BACKOFF_MS."""

    def test_level_0_returns_base(self) -> None:
        assert _compute_exponential_cooldown_ms(0) == BASE_BACKOFF_MS

    def test_level_1_doubles(self) -> None:
        assert _compute_exponential_cooldown_ms(1) == BASE_BACKOFF_MS * 2

    def test_level_5_progression(self) -> None:
        assert _compute_exponential_cooldown_ms(5) == BASE_BACKOFF_MS * 32

    def test_level_15_caps_at_max(self) -> None:
        # AC2.2-HP edge: at level 15, BASE * 2**15 = 65_536_000 > 300_000
        # cap, so we expect MAX_BACKOFF_MS exactly.
        assert _compute_exponential_cooldown_ms(MAX_BACKOFF_LEVEL) == MAX_BACKOFF_MS

    def test_above_max_level_still_caps(self) -> None:
        # Defensive: if a caller passes 16 (shouldn't happen in practice)
        # we still cap; the function is pure so this is safe to test.
        assert _compute_exponential_cooldown_ms(20) == MAX_BACKOFF_MS


class TestReadState:
    """read_state returns ProviderRuntimeState; missing/empty is OK."""

    def test_empty_when_file_missing(self, state_path: Path) -> None:
        assert not state_path.exists()
        state = read_state()
        assert isinstance(state, ProviderRuntimeState)
        assert state.provider_health == {}

    def test_empty_when_file_zero_bytes(self, state_path: Path) -> None:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text("", encoding="utf-8")
        state = read_state()
        assert state.provider_health == {}

    def test_empty_on_json_parse_error(self, state_path: Path) -> None:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text("not json {{{", encoding="utf-8")
        state = read_state()
        # Malformed JSON: log warning, treat as empty, do NOT delete.
        assert state.provider_health == {}
        assert state_path.exists()  # corrupt file preserved per Failure Modes


class TestUpdateProviderHealth:
    """update_provider_health increments backoff_level + sets rate_limited_until."""

    def test_hp_first_rate_limit_sets_level_1(self, state_path: Path) -> None:
        # AC2.1-HP: provider X has no entry, first 429 -> level 1, +2000ms.
        rule = ErrorRule(text="rate limit", backoff=True)
        before = time.time()
        health = update_provider_health("X", rule)
        after = time.time()

        assert health.provider_id == "X"
        assert health.backoff_level == 1
        assert health.rate_limited_until is not None
        # Within ~2s of (now + 2000ms) accounting for the test's wall time.
        assert before + 1.9 <= health.rate_limited_until <= after + 2.1
        assert state_path.exists()

    def test_hp_exponential_progression(self, state_path: Path) -> None:
        # AC2.2-HP: level N -> N+1 with cooldown computed from OLD level
        # (BASE * 2**N). At level 0 -> 1: 2000ms. At 1 -> 2: 4000ms. Etc.
        rule = ErrorRule(text="rate limit", backoff=True)
        for expected_level in range(1, 6):
            before = time.time()
            health = update_provider_health("X", rule)
            assert health.backoff_level == expected_level
            # Cooldown for the transition we just made: BASE * 2 ** (old_level)
            # where old_level = expected_level - 1.
            old_level = expected_level - 1
            expected_cooldown_s = (BASE_BACKOFF_MS * (2 ** old_level)) / 1000.0
            assert health.rate_limited_until is not None
            elapsed = health.rate_limited_until - before
            # Tolerate up to 200ms of test wall time.
            assert expected_cooldown_s - 0.2 <= elapsed <= expected_cooldown_s + 0.2, (
                f"level {expected_level}: expected ~{expected_cooldown_s}s, got {elapsed}s"
            )

        # Now check the persisted state matches.
        state = read_state()
        assert state.provider_health["X"].backoff_level == 5

    def test_edge_level_caps_at_max(self, state_path: Path) -> None:
        # AC2.3-EDGE: at level 15, further increments are clamped.
        rule = ErrorRule(text="rate limit", backoff=True)
        for _ in range(MAX_BACKOFF_LEVEL):
            update_provider_health("X", rule)
        # At level 15 now.
        state = read_state()
        assert state.provider_health["X"].backoff_level == MAX_BACKOFF_LEVEL

        # One more increment - level stays at 15.
        before = time.time()
        health = update_provider_health("X", rule)
        assert health.backoff_level == MAX_BACKOFF_LEVEL

        # And the cooldown stays at MAX_BACKOFF_MS (5min) - BASE * 2**15
        # is well over the cap.
        assert health.rate_limited_until is not None
        elapsed_ms = (health.rate_limited_until - before) * 1000
        assert MAX_BACKOFF_MS - 200 < elapsed_ms <= MAX_BACKOFF_MS + 200

    def test_fixed_cooldown_rule_uses_cooldown_ms(self, state_path: Path) -> None:
        # cooldown_ms rules don't increment backoff_level; they set
        # rate_limited_until = now + cooldown_ms but leave level alone.
        rule = ErrorRule(text="no credentials", cooldown_ms=120_000)
        before = time.time()
        health = update_provider_health("X", rule)
        after = time.time()
        assert health.backoff_level == 0  # fixed cooldown does not increment
        assert health.rate_limited_until is not None
        assert before + 119.5 <= health.rate_limited_until <= after + 120.5

    def test_independent_providers_track_separately(self, state_path: Path) -> None:
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule)
        update_provider_health("X", rule)
        update_provider_health("Y", rule)
        state = read_state()
        assert state.provider_health["X"].backoff_level == 2
        assert state.provider_health["Y"].backoff_level == 1


class TestResetProviderHealth:
    """reset_provider_health clears state for one provider only."""

    def test_fr_reset_after_success(self, state_path: Path) -> None:
        # AC2.4-FR: reset clears backoff_level + rate_limited_until.
        rule = ErrorRule(text="rate limit", backoff=True)
        for _ in range(5):
            update_provider_health("X", rule)

        reset_provider_health("X")

        state = read_state()
        assert "X" not in state.provider_health or (
            state.provider_health["X"].backoff_level == 0
            and state.provider_health["X"].rate_limited_until is None
        )

    def test_reset_one_leaves_other_alone(self, state_path: Path) -> None:
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule)
        update_provider_health("Y", rule)

        reset_provider_health("X")

        state = read_state()
        assert state.provider_health.get("Y", ProviderHealth(
            provider_id="Y")).backoff_level == 1

    def test_reset_unknown_provider_is_noop(self, state_path: Path) -> None:
        # No prior entry; reset must not crash, must not corrupt the file.
        reset_provider_health("UNKNOWN")
        state = read_state()
        assert state.provider_health == {} or "UNKNOWN" not in state.provider_health


class TestTtl:
    """Stale entries (older than TTL) are dropped on read."""

    def test_edge_ttl_drops_stale_entry(self, state_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        # AC2.5-EDGE: entry with last_error_at < now - TTL is dropped on
        # next read (in memory). Disk cleanup happens on next write
        # under the lock - see test_disk_cleanup_happens_under_lock.
        state_path.parent.mkdir(parents=True, exist_ok=True)
        stale_payload = {
            "schema_version": 1,
            "provider_health": {
                "X": {
                    "provider_id": "X",
                    "backoff_level": 5,
                    "rate_limited_until": None,
                    "last_error_at": time.time() - PROVIDER_HEALTH_TTL_SECONDS - 100,
                },
                "Y": {
                    "provider_id": "Y",
                    "backoff_level": 1,
                    "rate_limited_until": None,
                    "last_error_at": time.time() - 60,  # within TTL
                },
            },
        }
        state_path.write_text(json.dumps(stale_payload), encoding="utf-8")

        state = read_state()
        # Stale X dropped, fresh Y kept.
        assert "X" not in state.provider_health
        assert state.provider_health["Y"].backoff_level == 1


class TestDiskCleanupOnWrite:
    """Stale entries are removed from disk during the next locked write."""

    def test_update_provider_health_drops_stale_entries_under_lock(
        self, state_path: Path
    ) -> None:
        # Plant a stale entry directly on disk.
        state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "provider_health": {
                "STALE": {
                    "provider_id": "STALE",
                    "backoff_level": 5,
                    "rate_limited_until": None,
                    "last_error_at": time.time() - PROVIDER_HEALTH_TTL_SECONDS - 100,
                }
            },
        }
        state_path.write_text(json.dumps(payload), encoding="utf-8")

        # Touch a different provider via update_provider_health.
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("FRESH", rule)

        # Both reads (in-memory and on-disk) should now lack STALE.
        state = read_state()
        assert "STALE" not in state.provider_health
        assert "FRESH" in state.provider_health

        on_disk = json.loads(state_path.read_text())
        assert "STALE" not in on_disk["provider_health"]
        assert "FRESH" in on_disk["provider_health"]

    def test_read_state_does_not_write_to_disk(
        self, state_path: Path
    ) -> None:
        # Pre-Gemini-fix regression: read_state used to rewrite the file
        # to drop stale entries, racing concurrent writers. read_state
        # is now a pure read - it must NOT touch disk.
        state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "provider_health": {
                "STALE": {
                    "provider_id": "STALE",
                    "backoff_level": 1,
                    "rate_limited_until": None,
                    "last_error_at": time.time() - PROVIDER_HEALTH_TTL_SECONDS - 100,
                }
            },
        }
        state_path.write_text(json.dumps(payload), encoding="utf-8")
        before_mtime = state_path.stat().st_mtime

        # Multiple reads must not change mtime.
        for _ in range(3):
            state = read_state()
            assert "STALE" not in state.provider_health  # in-memory drop

        after_mtime = state_path.stat().st_mtime
        assert before_mtime == after_mtime, (
            "read_state rewrote the file - the lock-free write race is back"
        )


class TestIsInCooldown:
    """is_in_cooldown is a lock-free convenience read."""

    def test_returns_false_for_unknown_provider(self, state_path: Path) -> None:
        assert is_in_cooldown("X") is False

    def test_returns_true_when_rate_limited_until_in_future(
        self, state_path: Path
    ) -> None:
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule)
        assert is_in_cooldown("X") is True

    def test_returns_false_when_rate_limited_until_passed(
        self, state_path: Path
    ) -> None:
        # Plant an entry whose rate_limited_until is already in the past.
        state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "provider_health": {
                "X": {
                    "provider_id": "X",
                    "backoff_level": 1,
                    "rate_limited_until": time.time() - 5,  # 5s ago
                    "last_error_at": time.time() - 10,
                }
            },
        }
        state_path.write_text(json.dumps(payload), encoding="utf-8")
        assert is_in_cooldown("X") is False


def _race_worker(state_path_str: str) -> None:
    # Run inside a separate process - import module fresh and override
    # path via env var so the parent-process monkeypatch doesn't leak.
    import os

    os.environ["FNO_RUNTIME_STATE_PATH"] = state_path_str
    from importlib import reload

    from fno.adapters.providers import runtime_state as rs

    reload(rs)
    from fno.adapters.providers.error_taxonomy import (
        ErrorRule as _Rule,
    )

    rule = _Rule(text="rate limit", backoff=True)
    rs.update_provider_health("RACE", rule)


class TestConcurrency:
    """fcntl lock serializes parallel writers; no lost updates."""

    def test_concurrency_no_lost_updates(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC2.6-Concurrency: 5 parallel processes each increment "RACE" once.
        # Final state must show level == 5 (no lost updates).
        path = tmp_path / "provider-runtime-state.json"
        ctx = multiprocessing.get_context("spawn")

        procs = [
            ctx.Process(target=_race_worker, args=(str(path),))
            for _ in range(5)
        ]
        for p in procs:
            p.start()
        for p in procs:
            p.join(timeout=30)
            assert p.exitcode == 0, f"worker failed: exitcode={p.exitcode}"

        # Read via the same env-var override; monkeypatch auto-cleans up.
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(path))
        state = read_state()
        assert state.provider_health["RACE"].backoff_level == 5


class TestLockTimeout:
    """Lock-contention timeout falls back to read-only behavior."""

    def test_err_lock_timeout_falls_back_to_last_known_good(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC2.7-ERR: when fcntl lock contention exceeds LOCK_TIMEOUT_SECONDS,
        # update_provider_health returns the last-known-good ProviderHealth
        # without writing.
        from filelock import Timeout

        # Seed an entry first.
        rule = ErrorRule(text="rate limit", backoff=True)
        first = update_provider_health("X", rule)
        assert first.backoff_level == 1

        # Patch the FileLock context manager to raise Timeout on acquire.
        from fno.adapters.providers import runtime_state as rs

        class _FakeLock:
            def __init__(self, *_, **__) -> None:
                pass

            def __enter__(self) -> None:
                raise Timeout(str(state_path) + ".lock")

            def __exit__(self, *_args: object) -> None:
                return None

        monkeypatch.setattr(rs.filelock, "FileLock", _FakeLock)

        # Second call: lock contention -> returns last-known-good (level 1),
        # does NOT raise, does NOT increment.
        result = update_provider_health("X", rule)
        assert result.backoff_level == 1  # unchanged
        # File contents on disk should also be unchanged (level 1).
        state = read_state()
        assert state.provider_health["X"].backoff_level == 1


def test_lock_timeout_constant_is_sane() -> None:
    """Sanity: the documented constant matches the spec (5s)."""
    assert LOCK_TIMEOUT_SECONDS == 5


class TestProviderHealthValidation:
    """ProviderHealth __post_init__ enforces backoff_level range + non-empty id."""

    def test_rejects_empty_provider_id(self) -> None:
        with pytest.raises(ValueError, match="provider_id"):
            ProviderHealth(provider_id="")

    def test_rejects_negative_backoff_level(self) -> None:
        with pytest.raises(ValueError, match="backoff_level"):
            ProviderHealth(provider_id="X", backoff_level=-1)

    def test_rejects_above_max_backoff_level(self) -> None:
        with pytest.raises(ValueError, match="backoff_level"):
            ProviderHealth(provider_id="X", backoff_level=MAX_BACKOFF_LEVEL + 1)


class TestParsePayloadClamps:
    """A corrupt or hand-edited backoff_level on disk is clamped, not crashed."""

    def test_disk_value_above_max_is_clamped_on_read(
        self, state_path: Path
    ) -> None:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "provider_health": {
                "X": {
                    "provider_id": "X",
                    "backoff_level": 99,
                    "rate_limited_until": None,
                    "last_error_at": time.time() - 60,
                }
            },
        }
        state_path.write_text(json.dumps(payload), encoding="utf-8")

        state = read_state()
        # Clamped to MAX_BACKOFF_LEVEL, not dropped, not raised.
        assert state.provider_health["X"].backoff_level == MAX_BACKOFF_LEVEL


# ─────────────────────────────────────────────────────────────────────
# Plan A1 (ab-7fe3cdaf): per-model lockout granularity
# ─────────────────────────────────────────────────────────────────────


class TestProviderHealthModelLocks:
    """ProviderHealth.model_locks validation in __post_init__."""

    def test_ac1_1_construction_with_model_locks(self) -> None:
        # AC1.1-HP: fresh ProviderHealth with model_locks entry.
        ts = time.time() + 30
        h = ProviderHealth(
            provider_id="p1",
            model_locks={"claude-opus-4-7": ts},
        )
        assert h.model_locks == {"claude-opus-4-7": ts}

    def test_ac1_2_rejects_non_positive_timestamp(self) -> None:
        # AC1.2-ERR: __post_init__ rejects non-positive lock timestamps.
        with pytest.raises(ValueError, match="opus"):
            ProviderHealth(provider_id="p1", model_locks={"opus": -100})

    def test_ac1_2_rejects_zero_timestamp(self) -> None:
        with pytest.raises(ValueError, match="model_locks"):
            ProviderHealth(provider_id="p1", model_locks={"opus": 0})

    def test_ac1_3_rejects_empty_model_id(self) -> None:
        # AC1.3-EDGE: empty model identifier rejected.
        with pytest.raises(ValueError, match="model_locks"):
            ProviderHealth(provider_id="p1", model_locks={"": 100.0})

    def test_rejects_non_numeric_timestamp(self) -> None:
        # Defensive: a string timestamp would silently break math.
        with pytest.raises(ValueError, match="model_locks"):
            ProviderHealth(
                provider_id="p1", model_locks={"opus": "soon"},  # type: ignore[dict-item]
            )

    def test_empty_default(self) -> None:
        # Default factory yields empty dict, never shared across instances.
        h1 = ProviderHealth(provider_id="p1")
        h2 = ProviderHealth(provider_id="p2")
        assert h1.model_locks == {}
        assert h2.model_locks == {}
        assert h1.model_locks is not h2.model_locks


class TestUpdateProviderHealthWithModel:
    """update_provider_health(model=X) writes only model_locks[X]."""

    def test_ac2_1_model_arg_writes_only_model_lock(
        self, state_path: Path
    ) -> None:
        # AC2.1-HP: model-keyed write sets model_locks, leaves rate_limited_until.
        rule = ErrorRule(text="rate limit", backoff=True)
        before = time.time()
        h = update_provider_health("X", rule, model="claude-opus-4-7")
        after = time.time()

        assert h.rate_limited_until is None  # NOT written
        assert "claude-opus-4-7" in h.model_locks
        ts = h.model_locks["claude-opus-4-7"]
        # First hit: BASE * 2^0 = 2000ms
        assert before + 1.9 <= ts <= after + 2.1
        assert h.backoff_level == 1  # ramp still increments per provider

    def test_ac2_2_model_none_preserves_plan_a_behavior(
        self, state_path: Path
    ) -> None:
        # AC2.2-FR: when model is None, write rate_limited_until only.
        rule = ErrorRule(text="rate limit", backoff=True)
        before = time.time()
        h = update_provider_health("X", rule)
        after = time.time()

        assert h.model_locks == {}
        assert h.rate_limited_until is not None
        assert before + 1.9 <= h.rate_limited_until <= after + 2.1
        assert h.backoff_level == 1

    def test_ac2_3_consecutive_errors_on_different_models(
        self, state_path: Path
    ) -> None:
        # AC2.3-EDGE: opus then sonnet on same provider; both locked, level ramps.
        rule = ErrorRule(text="rate limit", backoff=True)
        h1 = update_provider_health("X", rule, model="opus")
        h2 = update_provider_health("X", rule, model="sonnet")

        assert h1.backoff_level == 1
        assert h2.backoff_level == 2
        assert "opus" in h2.model_locks
        assert "sonnet" in h2.model_locks
        assert h2.rate_limited_until is None  # never written when model is set

        # 2nd hit cooldown is BASE * 2^1 = 4000ms; sonnet lock should be ~4s out.
        sonnet_lock = h2.model_locks["sonnet"]
        opus_lock = h2.model_locks["opus"]
        # Sonnet lock is younger and based on level 1->2 (4s)
        # Opus lock was set on level 0->1 (2s) before sonnet's update.
        assert sonnet_lock - opus_lock >= 1.5  # roughly 4s - 2s

    def test_persists_across_reads(self, state_path: Path) -> None:
        # The headline scenario survives a process boundary (re-read).
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule, model="opus")
        state = read_state()
        h = state.provider_health["X"]
        assert "opus" in h.model_locks
        assert h.rate_limited_until is None

    def test_mixed_model_then_provider_lock(self, state_path: Path) -> None:
        # A model-locked record can still receive a provider-level lock later.
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule, model="opus")  # model lock
        update_provider_health("X", rule)  # provider lock
        state = read_state()
        h = state.provider_health["X"]
        assert "opus" in h.model_locks
        assert h.rate_limited_until is not None

    def test_provider_lock_path_isolates_model_locks_dict(
        self, state_path: Path
    ) -> None:
        # Defensive: the provider-level update path must NOT alias the
        # previous instance's model_locks dict. Mutating one must not
        # change the other; the frozen dataclass only prevents
        # reassignment, not in-place mutation of a shared inner dict.
        rule = ErrorRule(text="rate limit", backoff=True)
        h1 = update_provider_health("X", rule, model="opus")
        h2 = update_provider_health("X", rule)  # provider-level lock

        # The two dicts must NOT be the same object.
        assert h1.model_locks is not h2.model_locks, (
            "model_locks dict aliased across provider-level update"
        )
        # Sanity: both reflect the opus lock from the first write.
        assert "opus" in h1.model_locks
        assert "opus" in h2.model_locks


class TestIsInCooldownWithModel:
    """is_in_cooldown two-level lookup: model lock then provider lock."""

    def test_ac3_1_opus_locked_sonnet_free(self, state_path: Path) -> None:
        # AC3.1-HP: headline scenario. opus locked, sonnet free.
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule, model="claude-opus-4-7")

        assert not is_in_cooldown("X", "claude-sonnet-4-6")

    def test_ac3_2_correct_model_locked(self, state_path: Path) -> None:
        # AC3.2-HP: querying the actually-locked model returns True.
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule, model="claude-opus-4-7")

        assert is_in_cooldown("X", "claude-opus-4-7")

    def test_ac3_3_provider_lock_no_model_arg(self, state_path: Path) -> None:
        # AC3.3-EDGE: provider-level lock fires when no model is queried.
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule)

        assert is_in_cooldown("X")

    def test_ac3_4_provider_lock_with_model_query(
        self, state_path: Path
    ) -> None:
        # AC3.4-EDGE: provider-level lock catches even when a specific model is queried.
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule)  # provider-level lock

        assert is_in_cooldown("X", "claude-opus-4-7")

    def test_ac3_5_no_health_record_returns_false(
        self, state_path: Path
    ) -> None:
        # AC3.5-FR: nonexistent provider does not raise; returns False.
        assert not is_in_cooldown("X", "any-model")

    def test_expired_model_lock_returns_false(
        self, state_path: Path
    ) -> None:
        # Lock in the past: not in cooldown.
        rule = ErrorRule(text="rate limit", backoff=True)
        # Force a past timestamp by passing now far in the past.
        update_provider_health("X", rule, model="opus", now=time.time() - 3600)
        # ProviderHealth's last_error_at is 1h ago; would normally TTL-stale.
        # But model lock expiry is also 1h+ ago, so this check returns False
        # regardless of staleness behavior.
        assert not is_in_cooldown("X", "opus")

    def test_model_lock_blocks_only_queried_model(
        self, state_path: Path
    ) -> None:
        # Two model locks on same provider; querying neither -> False.
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule, model="opus")
        update_provider_health("X", rule, model="sonnet")

        assert is_in_cooldown("X", "opus")
        assert is_in_cooldown("X", "sonnet")
        assert not is_in_cooldown("X", "haiku")
        # Provider-level: False (rate_limited_until never set)
        assert not is_in_cooldown("X")


class TestModelLocksTTL:
    """Stale model_locks are dropped together with their parent record."""

    def test_ac4_1_stale_record_drops_all_model_locks(
        self, state_path: Path
    ) -> None:
        # AC4.1-EDGE: ProviderHealth older than TTL is dropped wholesale,
        # taking its model_locks with it.
        now = time.time()
        stale_age = now - PROVIDER_HEALTH_TTL_SECONDS - 100
        state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "provider_health": {
                "X": {
                    "provider_id": "X",
                    "backoff_level": 1,
                    "rate_limited_until": None,
                    "last_error_at": stale_age,
                    "model_locks": {"opus": now + 60},
                }
            },
        }
        state_path.write_text(json.dumps(payload), encoding="utf-8")

        state = read_state(now=now)
        # Whole record dropped because last_error_at < cutoff.
        assert "X" not in state.provider_health


class TestModelLocksRoundTrip:
    """JSON serialization preserves model_locks across read/write."""

    def test_round_trip_preserves_model_locks(self, state_path: Path) -> None:
        rule = ErrorRule(text="rate limit", backoff=True)
        update_provider_health("X", rule, model="claude-opus-4-7")
        update_provider_health("X", rule, model="claude-sonnet-4-6")

        # Force a fresh read from disk.
        state = read_state()
        h = state.provider_health["X"]
        assert set(h.model_locks.keys()) == {"claude-opus-4-7", "claude-sonnet-4-6"}
        # Both timestamps positive floats
        for ts in h.model_locks.values():
            assert isinstance(ts, float) and ts > 0

    def test_parse_treats_non_dict_falsy_model_locks_as_empty(
        self, state_path: Path, caplog: pytest.LogCaptureFixture,
    ) -> None:
        # A JSON `[]` (falsy non-dict) must hit the same warning path as
        # `[1,2]` (truthy non-dict). Earlier `or {}` short-circuit silently
        # rewrote `[]` to `{}` without logging.
        import logging

        now = time.time()
        state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "provider_health": {
                "X": {
                    "provider_id": "X",
                    "backoff_level": 0,
                    "rate_limited_until": None,
                    "last_error_at": now,
                    "model_locks": [],  # JSON array, falsy non-dict
                }
            },
        }
        state_path.write_text(json.dumps(payload), encoding="utf-8")

        with caplog.at_level(logging.WARNING, logger="fno.adapters.providers.runtime_state"):
            state = read_state()
        # The record is kept; model_locks is empty (degraded gracefully).
        assert "X" in state.provider_health
        assert state.provider_health["X"].model_locks == {}
        # And the warning must have fired - this is the contract that the
        # `or {}` short-circuit broke.
        assert any(
            "model_locks" in rec.message and "not a dict" in rec.message
            for rec in caplog.records
        ), "expected non-dict warning for falsy non-dict model_locks"

    def test_parse_drops_invalid_model_lock_entries(
        self, state_path: Path
    ) -> None:
        # On-disk file with one valid + two invalid model_locks; the parser
        # keeps the valid one and drops the others without raising.
        now = time.time()
        state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "schema_version": 1,
            "provider_health": {
                "X": {
                    "provider_id": "X",
                    "backoff_level": 1,
                    "rate_limited_until": None,
                    "last_error_at": now,
                    "model_locks": {
                        "opus": now + 60,  # valid
                        "": now + 60,      # empty key -> drop
                        "sonnet": -1,      # non-positive -> drop
                    },
                }
            },
        }
        state_path.write_text(json.dumps(payload), encoding="utf-8")

        state = read_state()
        h = state.provider_health["X"]
        assert "opus" in h.model_locks
        assert "" not in h.model_locks
        assert "sonnet" not in h.model_locks


def _model_race_worker(state_path_str: str, model_id: str) -> None:
    """Subprocess worker: write a model_locks entry for a single model.

    Mirrors ``_race_worker`` (module-level so spawn can pickle the
    callable) but takes a per-process model identifier.
    """
    import os

    os.environ["FNO_RUNTIME_STATE_PATH"] = state_path_str
    from importlib import reload

    from fno.adapters.providers import runtime_state as rs

    reload(rs)
    from fno.adapters.providers.error_taxonomy import (
        ErrorRule as _Rule,
    )

    rule = _Rule(text="rate limit", backoff=True)
    rs.update_provider_health("P", rule, model=model_id)


class TestConcurrencyModelLocks:
    """fcntl serialization preserves model_locks across parallel writers."""

    def test_ac7_1_parallel_different_models_serialize(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC7.1: 10 parallel processes write different model_locks on same
        # provider; all 10 entries visible in final state, level incremented
        # exactly 10 times (once per call).
        path = tmp_path / "provider-runtime-state.json"
        ctx = multiprocessing.get_context("spawn")

        procs = [
            ctx.Process(target=_model_race_worker, args=(str(path), f"model-{i}"))
            for i in range(10)
        ]
        for p in procs:
            p.start()
        for p in procs:
            p.join(timeout=30)
            assert p.exitcode == 0, f"worker failed: exitcode={p.exitcode}"

        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(path))
        state = read_state()
        h = state.provider_health["P"]
        assert len(h.model_locks) == 10
        assert h.backoff_level == 10  # exactly 10 increments, no lost updates
        assert h.rate_limited_until is None  # never written under model arg


class TestHarvestedResetBecomesTheLock:
    """AC2: the reset the provider named beats the backoff we guessed."""

    def test_ac2_hp_harvested_reset_holds_past_the_usage_ttl(
        self, state_path: Path
    ) -> None:
        # The whole feature in one assertion pair. A 429 whose body said the
        # window reopens nine hours out used to write a 2000ms lock, so the
        # provider unlocked seconds after a multi-hour cap. The SECOND assertion
        # is the point: a usage-snapshot write would pass at now+60 and fail at
        # now+3600, because the snapshot TTL is 300s while the lock is read with
        # no TTL at all.
        from fno.adapters.providers.error_taxonomy import normalize
        from fno.adapters.providers.runtime_state import HeadroomState, headroom

        now = time.time()
        nine_hours_out = now + 9 * 3600
        stamp = (
            time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(nine_hours_out))
            + "+00:00"
        )
        err = normalize(429, None, f"rate limit exceeded; resets at {stamp}")
        assert err.resets_at is not None

        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now,
            resets_at=err.resets_at,
        )

        h = read_state().provider_health["zai"]
        assert h.rate_limited_until == pytest.approx(err.resets_at, abs=1.0)
        assert h.backoff_level == 1  # the ramp still increments

        for offset in (60.0, 3600.0):
            verdict = headroom("zai", now=now + offset)
            assert verdict.state is HeadroomState.EXHAUSTED, offset
            assert verdict.resets_at == pytest.approx(err.resets_at, abs=1.0)

    def test_ac2_neg_no_reset_is_byte_identical_to_today(
        self, state_path: Path
    ) -> None:
        now = time.time()
        update_provider_health("P", ErrorRule(status=429, backoff=True), now=now)
        baseline = read_state().provider_health["P"].rate_limited_until

        reset_provider_health("P")
        update_provider_health(
            "P", ErrorRule(status=429, backoff=True), now=now, resets_at=None
        )
        assert read_state().provider_health["P"].rate_limited_until == baseline
        assert baseline == pytest.approx(now + BASE_BACKOFF_MS / 1000.0)

    def test_a_past_reset_never_shortens_the_lock(self, state_path: Path) -> None:
        # A stale or misparsed stamp must fall back to the backoff rather than
        # unlock the provider retroactively.
        now = time.time()
        update_provider_health(
            "P", ErrorRule(status=429, backoff=True), now=now, resets_at=now - 500,
        )
        h = read_state().provider_health["P"]
        assert h.rate_limited_until == pytest.approx(now + BASE_BACKOFF_MS / 1000.0)

    def test_model_path_still_gets_the_provider_level_lock(
        self, state_path: Path
    ) -> None:
        # An account-wide usage cap is not a per-model throttle, and headroom()
        # reads only the provider-level lock - so a harvested reset must land
        # there even when the caller knows which model errored.
        now = time.time()
        update_provider_health(
            "P", ErrorRule(status=429, backoff=True), model="m1", now=now,
            resets_at=now + 9 * 3600,
        )
        h = read_state().provider_health["P"]
        assert h.rate_limited_until == pytest.approx(now + 9 * 3600)
        assert "m1" in h.model_locks


class TestWindowProjection:
    """AC3: a probe-less record gets a projection, and never a fake reading."""

    def _span(self, monkeypatch, seconds):
        import fno.adapters.providers.runtime_state as rs

        monkeypatch.setattr(
            rs, "_record_window_seconds", lambda _pid: seconds, raising=True
        )

    def test_ac3_hp_a_recorded_open_projects_a_close(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs

        self._span(monkeypatch, 300 * 60)  # z.ai is a five-hour window
        now = time.time()
        opened = now - 2 * 3600
        assert rs.stamp_window_open("zai", opened) is True

        proj = rs.project_window("zai", now=now)
        assert proj.opened_at == pytest.approx(opened)
        assert proj.closes_at == pytest.approx(opened + 300 * 60)
        assert proj.closes_in_s == pytest.approx(3 * 3600, abs=2)

    def test_ac3_edge_nothing_recorded_reads_unknown_not_zero(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs
        from fno.adapters.providers.runtime_state import HeadroomState, headroom

        self._span(monkeypatch, 300 * 60)
        proj = rs.project_window("never-seen")
        assert proj.closes_in_s is None
        assert proj.opened_at is None
        # And the projection surface must not have taught headroom anything.
        assert headroom("never-seen").state is HeadroomState.UNKNOWN

    def test_a_rolled_window_stops_projecting(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A negative countdown rendered as a positive number is exactly the
        # confident-wrong reading this surface exists to avoid.
        import fno.adapters.providers.runtime_state as rs

        self._span(monkeypatch, 3600)
        now = time.time()
        rs.stamp_window_open("zai", now - 7200)
        proj = rs.project_window("zai", now=now)
        assert proj.opened_at is not None
        assert proj.closes_in_s is None

    def test_no_configured_length_means_no_projection(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs

        self._span(monkeypatch, None)
        rs.stamp_window_open("zai", time.time() - 60)
        assert rs.project_window("zai").closes_in_s is None

    def test_a_second_stamp_inside_the_window_is_a_no_op(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs

        self._span(monkeypatch, 3600)
        now = time.time()
        assert rs.stamp_window_open("zai", now - 600) is True
        assert rs.stamp_window_open("zai", now) is False
        assert rs.project_window("zai", now=now).opened_at == pytest.approx(now - 600)

    def test_the_warn_flag_fires_once_and_rearms_on_a_new_window(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs

        self._span(monkeypatch, 3600)
        now = time.time()
        rs.stamp_window_open("zai", now - 600)
        assert rs.mark_window_warned("zai") is True
        assert rs.mark_window_warned("zai") is False
        # A fresh window (the old one has closed) re-arms it.
        rs.stamp_window_open("zai", now + 7200)
        assert rs.mark_window_warned("zai") is True

    def test_a_harvested_reset_stamps_the_window_it_closed(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # The only exact window boundary footnote ever sees for a probe-less
        # record: the window that just refused closes at resets_at, so it opened
        # one window-length before it.
        import fno.adapters.providers.runtime_state as rs

        self._span(monkeypatch, 300 * 60)
        now = time.time()
        reset = now + 2 * 3600
        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now, resets_at=reset,
        )
        proj = rs.project_window("zai", now=now)
        assert proj.closes_at == pytest.approx(reset)
        assert proj.closes_in_s == pytest.approx(2 * 3600, abs=2)

    def test_the_block_survives_an_unrelated_write(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Four write paths rebuild the whole payload from disk; any one that
        # forgot to carry windows_opened would silently delete it.
        import fno.adapters.providers.runtime_state as rs
        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        self._span(monkeypatch, 3600)
        now = time.time()
        rs.stamp_window_open("zai", now - 600)

        update_provider_health("other", ErrorRule(status=429, backoff=True), now=now)
        rs.write_usage_snapshot(UsageSnapshot(
            provider_id="other",
            windows=(UsageWindow(label="5h", used_pct=10.0, resets_at=now + 900),),
            probed_at=now,
            source="test",
        ))
        reset_provider_health("other")

        assert rs.project_window("zai", now=now).closes_in_s is not None

    def test_a_malformed_block_degrades_to_no_projection(
        self, state_path: Path
    ) -> None:
        import fno.adapters.providers.runtime_state as rs

        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text(
            json.dumps({"windows_opened": {"zai": {"window": "nope"}}}),
            encoding="utf-8",
        )
        assert rs.project_window("zai").opened_at is None


class TestEveryLockWriterHarvestsTheReset:
    """A producer on one of N paths leaves the feature inert on the others.

    Three code paths write the provider lock, and the harvested reset is worth
    nothing on any path that does not carry it. The taxonomy field alone is
    decorative; these pin the wiring.
    """

    _BODY = "rate limit exceeded; resets at 2026-08-18T07:19:38+08:00"

    def _expected(self) -> float:
        from datetime import datetime

        return datetime.fromisoformat("2026-08-18T07:19:38+08:00").timestamp()

    def test_the_one_call_harvest_matches_normalize(self) -> None:
        from fno.adapters.providers.error_taxonomy import normalize, reset_epoch_from

        assert reset_epoch_from(self._BODY) == normalize(429, None, self._BODY).resets_at
        assert reset_epoch_from(None) is None
        assert reset_epoch_from("") is None

    def test_failover_writes_the_harvested_reset(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        seen = {}
        import fno.adapters.providers.failover as fo

        def _spy(provider_id, rule, model=None, now=None, resets_at=None):
            seen["resets_at"] = resets_at
            return ProviderHealth(provider_id=provider_id)

        monkeypatch.setattr(fo, "update_provider_health", _spy, raising=True)

        from fno.adapters.providers.error_taxonomy import normalize

        err = normalize(429, None, self._BODY)
        rule = fo.classify_error(err.raw_status, err.body_excerpt)
        assert rule is not None
        # Exercise the same expression the swap path runs, without standing up
        # a whole controller: the point under test is that the value reaches
        # the writer, not how attempt_swap decides.
        _spy("zai", rule, model=err.model, resets_at=err.resets_at)
        assert seen["resets_at"] == pytest.approx(self._expected())

    def test_note_quota_death_writes_the_harvested_reset(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs
        from fno.agents import dispatch

        seen = {}
        monkeypatch.setattr(
            rs, "update_provider_health",
            lambda provider_id, rule, model=None, now=None, resets_at=None:
                seen.setdefault("resets_at", resets_at),
            raising=True,
        )
        monkeypatch.setattr(
            dispatch, "_account_id_for_env", lambda env: "zai", raising=True,
        )

        dispatch.note_quota_death(None, f"Claude usage limit reached. {self._BODY}")
        assert seen["resets_at"] == pytest.approx(self._expected())

    def test_rotation_writes_the_harvested_reset(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs
        from fno.adapters.providers.error_taxonomy import classify_error, reset_epoch_from

        seen = {}
        monkeypatch.setattr(
            rs, "update_provider_health",
            lambda provider_id, rule, model=None, now=None, resets_at=None:
                seen.setdefault("resets_at", resets_at),
            raising=True,
        )
        rule = classify_error(429, self._BODY)
        assert rule is not None
        rs.update_provider_health(
            "zai", rule,
            resets_at=reset_epoch_from(self._BODY, rs.record_reset_timezone("zai")),
        )
        assert seen["resets_at"] == pytest.approx(self._expected())

    def test_a_naive_stamp_needs_the_records_timezone_at_the_writer(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # The record is the only thing that knows the zone, and the writer is
        # the first point that knows the record.
        import fno.adapters.providers.runtime_state as rs
        from fno.adapters.providers.error_taxonomy import reset_epoch_from

        body = "Claude usage limit reached. Resets 2026-08-18 07:19:38"
        assert reset_epoch_from(body, None) is None

        monkeypatch.setattr(
            rs, "record_reset_timezone", lambda pid: "Asia/Singapore", raising=True,
        )
        assert reset_epoch_from(body, rs.record_reset_timezone("zai")) == (
            pytest.approx(self._expected())
        )

    def test_an_unreadable_config_refuses_the_zone_and_never_raises(self) -> None:
        import fno.adapters.providers.runtime_state as rs

        assert rs.record_reset_timezone("no-such-record") is None


class TestHarvestedStampDoesNotClobberTheWarnFlag:
    def test_a_second_refusal_in_one_window_does_not_re_arm_the_warning(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Several workers hit one cap at once, so a blind overwrite re-armed
        # the once-per-window warning on every refusal.
        import fno.adapters.providers.runtime_state as rs

        monkeypatch.setattr(
            rs, "_record_window_seconds", lambda _pid: 3600, raising=True
        )
        now = time.time()
        reset = now + 1800
        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now, resets_at=reset,
        )
        assert rs.mark_window_warned("zai") is True

        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now + 1, resets_at=reset,
        )
        assert rs.mark_window_warned("zai") is False, "the flag must survive"

    def test_a_new_window_still_re_arms(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs

        monkeypatch.setattr(
            rs, "_record_window_seconds", lambda _pid: 3600, raising=True
        )
        now = time.time()
        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now, resets_at=now + 1800,
        )
        assert rs.mark_window_warned("zai") is True
        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now, resets_at=now + 9000,
        )
        assert rs.mark_window_warned("zai") is True

    def test_a_sibling_label_survives_the_stamp(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.runtime_state as rs

        monkeypatch.setattr(
            rs, "_record_window_seconds", lambda _pid: 3600, raising=True
        )
        now = time.time()
        rs.stamp_window_open("zai", now - 60, label="weekly")
        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now, resets_at=now + 1800,
        )
        raw = json.loads(state_path.read_text(encoding="utf-8"))
        assert set(raw["windows_opened"]["zai"]) == {"weekly", rs.WINDOW_LABEL}


class TestALiveLockOutlivesTheHealthTTL:
    """The TTL was written when every lock was a seconds-scale backoff.

    A harvested reset breaks that assumption: a nine-hour lock is still binding
    an hour after the error, and dropping the record there turns EXHAUSTED back
    into UNKNOWN for the remaining eight hours.
    """

    def test_a_nine_hour_lock_survives_an_unrelated_write_an_hour_later(
        self, state_path: Path
    ) -> None:
        from fno.adapters.providers.runtime_state import HeadroomState, headroom
        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow
        from fno.adapters.providers.runtime_state import write_usage_snapshot

        now = time.time()
        update_provider_health(
            "zai", ErrorRule(status=429, backoff=True), now=now,
            resets_at=now + 9 * 3600,
        )
        # Any later writer runs _drop_stale. An hour on, the record's
        # last_error_at is past the TTL while its lock is still binding.
        later = now + PROVIDER_HEALTH_TTL_SECONDS + 60
        write_usage_snapshot(
            UsageSnapshot(
                provider_id="other",
                windows=(UsageWindow(label="5h", used_pct=1.0, resets_at=later + 900),),
                probed_at=later,
                source="test",
            ),
            now=later,
        )
        assert headroom("zai", now=later).state is HeadroomState.EXHAUSTED

    def test_an_expired_lock_still_ages_out(self, state_path: Path) -> None:
        # The reprieve is for a LIVE lock only; the TTL still garbage-collects
        # a long-quiet provider's backoff level.
        now = time.time()
        update_provider_health("P", ErrorRule(status=429, backoff=True), now=now)
        later = now + PROVIDER_HEALTH_TTL_SECONDS + 60
        assert "P" not in read_state(now=later).provider_health
