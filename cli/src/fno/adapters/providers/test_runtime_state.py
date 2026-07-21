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
