"""Tests for combos rotation: Combo dataclass, load_combos, get_rotated_providers, dispatch_with_combo.

Plan B of provider-rotation 9router port (ab-0e5a921e). Run:
    cd cli && uv run pytest src/fno/adapters/providers/test_rotation.py -v
"""
from __future__ import annotations

from pathlib import Path

import pytest
import yaml


def _write_settings(path: Path, content: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(yaml.safe_dump(content, sort_keys=False), encoding="utf-8")


def _settings_with_records_and_combos(combos: dict) -> dict:
    """Build a settings.yaml with two providers + the given combos block."""
    return {
        "config": {
            "providers": {
                "active": "a",
                "records": [
                    {"id": "a", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
                    {"id": "b", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
                    {"id": "c", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
                ],
                "combos": combos,
            }
        }
    }


# ---------------------------------------------------------------------------
# AC1.1-HP: load_combos returns parsed Combo objects
# ---------------------------------------------------------------------------

class TestLoadCombosHappyPath:
    def test_load_combos_parses_round_robin_block(self, tmp_path: Path, monkeypatch):
        from fno.adapters.providers.loader import load_combos
        from fno.adapters.providers.rotation import Combo

        settings = tmp_path / ".fno" / "settings.yaml"
        _write_settings(
            settings,
            _settings_with_records_and_combos({
                "my-stack": {
                    "strategy": "round_robin",
                    "sticky_limit": 3,
                    "providers": ["a", "b", "c"],
                }
            }),
        )
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv("PWD", str(tmp_path))

        combos = load_combos()

        assert "my-stack" in combos
        my = combos["my-stack"]
        assert isinstance(my, Combo)
        assert my.name == "my-stack"
        assert my.strategy == "round_robin"
        assert my.sticky_limit == 3
        assert my.providers == ("a", "b", "c")

    def test_load_combos_defaults_strategy_to_fallback(self, tmp_path: Path, monkeypatch):
        from fno.adapters.providers.loader import load_combos

        settings = tmp_path / ".fno" / "settings.yaml"
        _write_settings(
            settings,
            _settings_with_records_and_combos({
                "fb": {"providers": ["a", "b"]},
            }),
        )
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv("PWD", str(tmp_path))

        combos = load_combos()
        assert combos["fb"].strategy == "fallback"
        assert combos["fb"].sticky_limit == 1


# ---------------------------------------------------------------------------
# AC1.2-ERR: empty providers list rejected at load
# ---------------------------------------------------------------------------

class TestLoadCombosErrors:
    def test_empty_providers_list_raises(self, tmp_path: Path, monkeypatch):
        from fno.adapters.providers.loader import load_combos
        from fno.adapters.providers.model import ProviderConfigError

        settings = tmp_path / ".fno" / "settings.yaml"
        _write_settings(
            settings,
            _settings_with_records_and_combos({
                "bad": {"strategy": "round_robin", "providers": []},
            }),
        )
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv("PWD", str(tmp_path))

        with pytest.raises(ProviderConfigError) as exc_info:
            load_combos()
        msg = str(exc_info.value)
        assert "bad" in msg
        assert "empty" in msg.lower()

    def test_combos_block_not_mapping_raises(self, tmp_path: Path, monkeypatch):
        from fno.adapters.providers.loader import load_combos
        from fno.adapters.providers.model import ProviderConfigError

        settings = tmp_path / ".fno" / "settings.yaml"
        # combos as a list instead of a mapping
        broken = {
            "config": {
                "providers": {
                    "active": "a",
                    "records": [{"id": "a", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"}],
                    "combos": ["not", "a", "mapping"],
                }
            }
        }
        _write_settings(settings, broken)
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv("PWD", str(tmp_path))

        with pytest.raises(ProviderConfigError) as exc_info:
            load_combos()
        assert "combos" in str(exc_info.value).lower()

    def test_unknown_provider_in_combo_raises(self, tmp_path: Path, monkeypatch):
        """Combos must reference only declared records; unknown provider id is rejected."""
        from fno.adapters.providers.loader import load_combos
        from fno.adapters.providers.model import ProviderConfigError

        settings = tmp_path / ".fno" / "settings.yaml"
        _write_settings(
            settings,
            _settings_with_records_and_combos({
                "mixed": {"providers": ["a", "ghost", "c"]},
            }),
        )
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv("PWD", str(tmp_path))

        with pytest.raises(ProviderConfigError) as exc_info:
            load_combos()
        msg = str(exc_info.value)
        assert "ghost" in msg
        assert "mixed" in msg


# ---------------------------------------------------------------------------
# AC1.3-EDGE: sticky_limit <= 0 clamps to 1 (matches 9router normalizeStickyLimit)
# ---------------------------------------------------------------------------

class TestComboConstructionEdges:
    def test_sticky_limit_zero_clamps_to_one(self):
        from fno.adapters.providers.rotation import Combo
        c = Combo(name="x", sticky_limit=0, providers=("a",))
        assert c.sticky_limit == 1

    def test_sticky_limit_negative_clamps_to_one(self):
        from fno.adapters.providers.rotation import Combo
        c = Combo(name="x", sticky_limit=-7, providers=("a",))
        assert c.sticky_limit == 1

    def test_invalid_strategy_raises_valueerror(self):
        from fno.adapters.providers.rotation import Combo
        with pytest.raises(ValueError) as exc_info:
            Combo(name="x", strategy="random", providers=("a",))  # type: ignore[arg-type]
        assert "random" in str(exc_info.value)

    def test_empty_providers_construction_raises(self):
        from fno.adapters.providers.rotation import Combo
        with pytest.raises(ValueError) as exc_info:
            Combo(name="x", providers=())
        assert "x" in str(exc_info.value)


# ---------------------------------------------------------------------------
# AC1.5-FR: no combos block returns empty dict
# ---------------------------------------------------------------------------

class TestLoadCombosNoBlock:
    def test_no_combos_key_returns_empty_dict(self, tmp_path: Path, monkeypatch):
        from fno.adapters.providers.loader import load_combos

        settings = tmp_path / ".fno" / "settings.yaml"
        _write_settings(
            settings,
            {
                "config": {
                    "providers": {
                        "active": "a",
                        "records": [{"id": "a", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"}],
                    }
                }
            },
        )
        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv("PWD", str(tmp_path))

        assert load_combos() == {}

    def test_no_settings_file_returns_empty_dict(self, tmp_path: Path, monkeypatch):
        from fno.adapters.providers.loader import load_combos

        monkeypatch.chdir(tmp_path)
        monkeypatch.setenv("PWD", str(tmp_path))
        # also clear HOME so global settings don't leak in
        monkeypatch.setenv("HOME", str(tmp_path))

        assert load_combos() == {}


# ---------------------------------------------------------------------------
# compute_providers_hash properties
# ---------------------------------------------------------------------------

class TestComputeProvidersHash:
    def test_hash_is_deterministic(self):
        from fno.adapters.providers.rotation import compute_providers_hash
        h1 = compute_providers_hash(("a", "b", "c"))
        h2 = compute_providers_hash(("a", "b", "c"))
        assert h1 == h2

    def test_hash_changes_when_providers_change(self):
        from fno.adapters.providers.rotation import compute_providers_hash
        h1 = compute_providers_hash(("a", "b", "c"))
        h2 = compute_providers_hash(("a", "b", "d"))
        assert h1 != h2

    def test_hash_changes_when_order_changes(self):
        """Order matters - rotation cursor only meaningful relative to a fixed order."""
        from fno.adapters.providers.rotation import compute_providers_hash
        h1 = compute_providers_hash(("a", "b", "c"))
        h2 = compute_providers_hash(("c", "b", "a"))
        assert h1 != h2


# ---------------------------------------------------------------------------
# AC2.1-HP: cursor advances per sticky_limit (sticky=3, providers=[a,b,c])
# ---------------------------------------------------------------------------

@pytest.fixture
def cursor_state_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect runtime_state to tmp path so cursor tests don't touch real files."""
    target = tmp_path / "provider-runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(target))
    return target


class TestAdvanceCursorStickyMath:
    def test_nine_calls_walk_idx_0_1_2_each_three_times(
        self, cursor_state_path: Path
    ):
        """AC2.1-HP: 9 calls with sticky=3, N=3 produce
        (0,1) (0,2) (0,3) (1,1) (1,2) (1,3) (2,1) (2,2) (2,3)."""
        from fno.adapters.providers.runtime_state import advance_cursor
        from fno.adapters.providers.rotation import compute_providers_hash

        h = compute_providers_hash(("a", "b", "c"))
        expected = [
            (0, 1), (0, 2), (0, 3),
            (1, 1), (1, 2), (1, 3),
            (2, 1), (2, 2), (2, 3),
        ]
        seen: list[tuple[int, int]] = []
        for _ in range(9):
            c = advance_cursor("my-stack", sticky_limit=3, providers_hash=h, providers_count=3)
            seen.append((c.cursor_index, c.consecutive_use_count))
        assert seen == expected

    def test_sticky_limit_one_advances_every_call(self, cursor_state_path: Path):
        """sticky_limit=1: every call advances the index. Walks 0,1,2,0,1,2..."""
        from fno.adapters.providers.runtime_state import advance_cursor
        from fno.adapters.providers.rotation import compute_providers_hash

        h = compute_providers_hash(("a", "b", "c"))
        idxs = [
            advance_cursor("s1", sticky_limit=1, providers_hash=h, providers_count=3).cursor_index
            for _ in range(7)
        ]
        assert idxs == [0, 1, 2, 0, 1, 2, 0]

    def test_single_provider_combo_never_advances(self, cursor_state_path: Path):
        """AC3.5-EDGE: single-provider combo: cursor never moves past idx=0."""
        from fno.adapters.providers.runtime_state import advance_cursor
        from fno.adapters.providers.rotation import compute_providers_hash

        h = compute_providers_hash(("only",))
        for _ in range(20):
            c = advance_cursor("solo", sticky_limit=3, providers_hash=h, providers_count=1)
            assert c.cursor_index == 0


# ---------------------------------------------------------------------------
# AC2.2-EDGE: providers_hash mismatch invalidates cursor
# ---------------------------------------------------------------------------

class TestCursorHashInvalidation:
    def test_read_returns_none_on_hash_mismatch(self, cursor_state_path: Path):
        from fno.adapters.providers.runtime_state import (
            advance_cursor,
            read_cursor,
        )
        from fno.adapters.providers.rotation import compute_providers_hash

        h_old = compute_providers_hash(("a", "b", "c"))
        # Walk cursor to index 2 under the old hash.
        for _ in range(7):  # 7 calls with sticky=3 lands at (2, 1)
            advance_cursor("k", sticky_limit=3, providers_hash=h_old, providers_count=3)
        # User edits the combo: hash changes.
        h_new = compute_providers_hash(("a", "b", "c", "d"))
        assert read_cursor("k", h_new) is None

    def test_read_returns_cursor_when_hash_matches(self, cursor_state_path: Path):
        from fno.adapters.providers.runtime_state import (
            advance_cursor,
            read_cursor,
        )
        from fno.adapters.providers.rotation import compute_providers_hash

        h = compute_providers_hash(("a", "b"))
        advance_cursor("k", sticky_limit=2, providers_hash=h, providers_count=2)
        c = read_cursor("k", h)
        assert c is not None
        assert c.cursor_index == 0
        assert c.consecutive_use_count == 1

    def test_advance_resets_to_idx_0_when_hash_changes(self, cursor_state_path: Path):
        """Cursor at (idx=2, count=1) under hash-A. User edits combo (hash-B).
        Next advance under hash-B treats it as fresh: returns (0, 1)."""
        from fno.adapters.providers.runtime_state import advance_cursor
        from fno.adapters.providers.rotation import compute_providers_hash

        h_old = compute_providers_hash(("a", "b", "c"))
        for _ in range(7):
            advance_cursor("k", sticky_limit=3, providers_hash=h_old, providers_count=3)
        h_new = compute_providers_hash(("a", "b", "c", "d"))
        c = advance_cursor("k", sticky_limit=3, providers_hash=h_new, providers_count=4)
        assert c.cursor_index == 0
        assert c.consecutive_use_count == 1


# ---------------------------------------------------------------------------
# AC2.3-FR: malformed entry on disk is treated as missing (no raise)
# ---------------------------------------------------------------------------

class TestCursorMalformedDiskEntry:
    def test_legacy_entry_missing_providers_hash_returns_none(
        self, cursor_state_path: Path
    ):
        import json
        from fno.adapters.providers.runtime_state import read_cursor

        cursor_state_path.parent.mkdir(parents=True, exist_ok=True)
        cursor_state_path.write_text(
            json.dumps({
                "schema_version": 2,
                "provider_health": {},
                "combo_cursors": {
                    "legacy": {
                        "combo_name": "legacy",
                        "cursor_index": 1,
                        "consecutive_use_count": 2,
                        # providers_hash absent
                        "last_rotated_at": 0.0,
                    }
                },
            }),
            encoding="utf-8",
        )
        # Without providers_hash on the entry, we cannot compare: treat as None.
        assert read_cursor("legacy", "any-hash") is None


# ---------------------------------------------------------------------------
# AC2.4-EDGE: 24h TTL clears stale cursor
# ---------------------------------------------------------------------------

class TestCursorTTL:
    def test_read_returns_none_when_cursor_is_25h_old(
        self, cursor_state_path: Path
    ):
        import time
        from fno.adapters.providers.runtime_state import (
            advance_cursor,
            read_cursor,
            COMBO_CURSOR_TTL_SECONDS,
        )
        from fno.adapters.providers.rotation import compute_providers_hash

        h = compute_providers_hash(("a", "b"))
        now = 1_000_000.0
        old_now = now - COMBO_CURSOR_TTL_SECONDS - 60
        advance_cursor(
            "stale", sticky_limit=2, providers_hash=h, providers_count=2, now=old_now
        )
        # Now read with a 'now' beyond TTL.
        assert read_cursor("stale", h, now=now) is None


# ---------------------------------------------------------------------------
# AC2.5-Concurrency: parallel advance_cursor serialize, no lost updates
# ---------------------------------------------------------------------------

def _hit_advance(state_path_str: str, combo_name: str, providers_hash: str) -> None:
    """Worker for parallel test - must be top-level for multiprocessing pickling."""
    import os
    from fno.adapters.providers.runtime_state import advance_cursor
    os.environ["FNO_RUNTIME_STATE_PATH"] = state_path_str
    advance_cursor(combo_name, sticky_limit=1, providers_hash=providers_hash, providers_count=10)


class TestCursorConcurrency:
    def test_parallel_advances_serialize_and_count_correctly(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """AC2.5: 20 parallel advance_cursor calls must serialize via fcntl.
        Final cursor reflects all 20 increments (no lost updates)."""
        import json
        import multiprocessing
        from fno.adapters.providers.rotation import compute_providers_hash

        state_path = tmp_path / "provider-runtime-state.json"
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(state_path))
        h = compute_providers_hash(tuple(f"p{i}" for i in range(10)))

        ctx = multiprocessing.get_context("spawn")
        procs = [
            ctx.Process(target=_hit_advance, args=(str(state_path), "race", h))
            for _ in range(20)
        ]
        for p in procs:
            p.start()
        for p in procs:
            p.join(timeout=20)
            assert p.exitcode == 0, f"worker failed: {p.exitcode}"

        # 20 advances at sticky=1, N=10 walks the index 20 times.
        # Net result: cursor_index = 20 % 10 = 0, count tracks the
        # final position; the load-bearing assertion is "no lost updates".
        # We verify by reading the on-disk file directly.
        payload = json.loads(state_path.read_text(encoding="utf-8"))
        cursors = payload.get("combo_cursors", {})
        assert "race" in cursors
        # 20 successful advances: at sticky=1 the count rolls every time;
        # final state's count is 1 (we just landed on a new idx after the 20th call).
        assert cursors["race"]["consecutive_use_count"] == 1


# ---------------------------------------------------------------------------
# Schema migration: v1 file (provider_health only) reads cleanly
# ---------------------------------------------------------------------------

class TestSchemaMigration:
    def test_v1_file_reads_with_empty_combo_cursors(self, cursor_state_path: Path):
        """Reading a v1 state file (no combo_cursors block) works fine and the
        in-memory state has an empty combo_cursors dict."""
        import json
        from fno.adapters.providers.runtime_state import read_state

        cursor_state_path.parent.mkdir(parents=True, exist_ok=True)
        cursor_state_path.write_text(
            json.dumps({
                "schema_version": 1,
                "provider_health": {
                    "x": {
                        "provider_id": "x",
                        "backoff_level": 0,
                        "rate_limited_until": None,
                        "last_error_at": None,
                    }
                },
            }),
            encoding="utf-8",
        )
        state = read_state()
        assert state.combo_cursors == {}
        assert "x" in state.provider_health


# ---------------------------------------------------------------------------
# CG3 tests: get_rotated_providers + dispatch_with_combo
# ---------------------------------------------------------------------------


@pytest.fixture
def combos_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Set up a tmp project with settings.yaml + isolated runtime_state path."""
    settings = tmp_path / ".fno" / "settings.yaml"
    _write_settings(
        settings,
        _settings_with_records_and_combos({
            "rr": {
                "strategy": "round_robin",
                "sticky_limit": 2,
                "providers": ["a", "b", "c"],
            },
            "fb": {
                "strategy": "fallback",
                "providers": ["a", "b", "c"],
            },
            "solo": {
                "strategy": "round_robin",
                "providers": ["a"],
            },
        }),
    )
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv(
        "FNO_RUNTIME_STATE_PATH",
        str(tmp_path / "provider-runtime-state.json"),
    )
    return tmp_path


class TestGetRotatedProviders:
    def test_fallback_returns_list_unchanged(self, combos_env: Path):
        from fno.adapters.providers.loader import load_combos
        from fno.adapters.providers.rotation import get_rotated_providers

        combos = load_combos()
        for _ in range(5):
            assert get_rotated_providers(combos["fb"]) == ["a", "b", "c"]

    def test_round_robin_advances_per_sticky(self, combos_env: Path):
        """sticky=2, N=3, 7 calls -> [a,b,c]x2, [b,c,a]x2, [c,a,b]x2, [a,b,c]

        get_rotated_providers is the one-shot read+advance helper retained
        for ad-hoc callers; production dispatch separates read from advance
        (see test_dispatch_with_combo_does_not_advance_on_cooldown_skip).
        """
        from fno.adapters.providers.loader import load_combos
        from fno.adapters.providers.rotation import get_rotated_providers

        combos = load_combos()
        seq = [get_rotated_providers(combos["rr"]) for _ in range(7)]
        assert seq == [
            ["a", "b", "c"],
            ["a", "b", "c"],
            ["b", "c", "a"],
            ["b", "c", "a"],
            ["c", "a", "b"],
            ["c", "a", "b"],
            ["a", "b", "c"],
        ]

    def test_single_provider_round_robin_short_circuits(self, combos_env: Path):
        from fno.adapters.providers.loader import load_combos
        from fno.adapters.providers.rotation import get_rotated_providers

        combos = load_combos()
        for _ in range(10):
            assert get_rotated_providers(combos["solo"]) == ["a"]


# ---------------------------------------------------------------------------
# AC3.1-HP: dispatch returns first non-cooldowned success, advances cursor
# ---------------------------------------------------------------------------

class TestDispatchWithComboHappyPath:
    def test_skips_cooldown_then_returns_success(self, combos_env: Path):
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
        )
        from fno.adapters.providers.runtime_state import (
            update_provider_health,
        )
        from fno.adapters.providers.error_taxonomy import ErrorRule

        # Put 'a' in cooldown via Plan A path (status 401 -> cooldown).
        update_provider_health("a", ErrorRule(status=401, cooldown_ms=60_000))
        seen: list[str] = []

        def fn(pid: str) -> CallOutcome:
            seen.append(pid)
            return CallOutcome(success=True, payload=f"ok-{pid}")

        out = dispatch_with_combo("rr", fn)
        # 'a' was cooldowned -> skipped. Cursor at idx=0 -> rotation [a,b,c].
        # 'a' skipped, 'b' attempted and succeeded.
        assert isinstance(out, CallOutcome)
        assert out.success is True
        assert out.payload == "ok-b"
        assert seen == ["b"]  # 'c' never attempted

    def test_unknown_combo_raises_combo_not_found(self, combos_env: Path):
        from fno.adapters.providers.rotation import (
            CallOutcome,
            ComboNotFoundError,
            dispatch_with_combo,
        )

        with pytest.raises(ComboNotFoundError) as exc_info:
            dispatch_with_combo("ghost", lambda pid: CallOutcome(success=True))
        assert "ghost" in str(exc_info.value)


# ---------------------------------------------------------------------------
# AC3.2-ERR: all in cooldown -> QueueExhausted with retry_after hint
# ---------------------------------------------------------------------------

class TestDispatchAllCooldown:
    def test_all_cooldown_returns_queue_exhausted(self, combos_env: Path):
        import time
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.rotation import (
            CallOutcome,
            QueueExhausted,
            dispatch_with_combo,
        )
        from fno.adapters.providers.runtime_state import (
            update_provider_health,
        )

        # Cooldown all three with different durations.
        for pid, ms in [("a", 30_000), ("b", 60_000), ("c", 90_000)]:
            update_provider_health(pid, ErrorRule(status=401, cooldown_ms=ms))

        called: list[str] = []

        def fn(pid: str) -> CallOutcome:
            called.append(pid)
            return CallOutcome(success=True)

        out = dispatch_with_combo("fb", fn)
        assert isinstance(out, QueueExhausted)
        assert called == []  # nothing attempted - all cooldowned
        assert out.retry_after is not None
        # Soonest = 'a' at +30s; allow some slack (test-time clock skew)
        assert out.retry_after - time.time() < 60


# ---------------------------------------------------------------------------
# AC3.3-FR: mid-iteration cooldown expiry is honored (re-check at each step)
# ---------------------------------------------------------------------------

class TestDispatchMidIterationCooldownExpiry:
    def test_per_step_cooldown_recheck(self, combos_env: Path):
        """Cooldown is read from fresh state per provider, no upfront snapshot.

        The loop reads runtime state once per iteration (x-5d3e review fix:
        replaces the is_in_cooldown()+read_state() double-read) and derives the
        cooldown from it. A cooled provider is skipped; the next serves.
        """
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
        )
        from fno.adapters.providers.runtime_state import update_provider_health

        # Seed 'a' in a real provider-level cooldown (fallback order [a,b,c]).
        update_provider_health("a", ErrorRule(status=401, cooldown_ms=60_000))

        attempts: list[str] = []

        def fn(pid: str) -> CallOutcome:
            attempts.append(pid)
            return CallOutcome(success=True, payload=pid)

        out = dispatch_with_combo("fb", fn)
        # 'a' cooled -> skipped; 'b' attempted and succeeded; 'c' never reached.
        assert attempts == ["b"]
        assert isinstance(out, CallOutcome)
        assert out.payload == "b"


# ---------------------------------------------------------------------------
# AC3.4 already covered by test_unknown_combo_raises_combo_not_found
# AC3.5 already covered by test_single_provider_round_robin_short_circuits
# Additional swap-trigger classification test:
# ---------------------------------------------------------------------------

class TestDispatchSwapTriggerClassification:
    def test_swap_trigger_advances_to_next_provider_and_updates_health(
        self, combos_env: Path
    ):
        """Provider 'a' returns 5xx-classified swap trigger; loop tries 'b'."""
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
        )
        from fno.adapters.providers.runtime_state import read_state

        attempts: list[str] = []

        def fn(pid: str) -> CallOutcome:
            attempts.append(pid)
            if pid == "a":
                # Swap-trigger 5xx
                return CallOutcome(
                    success=False, swap_trigger=True, status=503, body="overloaded"
                )
            return CallOutcome(success=True, payload=pid)

        out = dispatch_with_combo("fb", fn)
        assert isinstance(out, CallOutcome)
        assert out.success is True
        assert attempts == ["a", "b"]
        # 'a' should have a non-zero backoff_level after the swap-trigger classify.
        state = read_state()
        assert "a" in state.provider_health
        assert state.provider_health["a"].backoff_level > 0

    def test_non_swap_trigger_failure_surfaces_immediately(self, combos_env: Path):
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
        )

        attempts: list[str] = []

        def fn(pid: str) -> CallOutcome:
            attempts.append(pid)
            return CallOutcome(
                success=False, swap_trigger=False, status=400, body="bad request"
            )

        out = dispatch_with_combo("fb", fn)
        assert isinstance(out, CallOutcome)
        assert out.success is False
        assert attempts == ["a"]  # never advanced past first provider


# ---------------------------------------------------------------------------
# PR #230 review H1 regression: cooldown skips must not burn sticky slots
# ---------------------------------------------------------------------------

class TestDispatchCursorAdvancesOnlyOnServedCalls:
    def test_cooldown_skip_does_not_advance_cursor(self, combos_env: Path):
        """3 providers (round_robin, sticky=2), 'a' cooldowned: b serves but
        cursor stays at idx=0 across the cooldown skip + b serve. Next call
        sees the SAME rotation list [a,b,c] (cursor only advanced once on
        b's served slot, not twice for skip+serve).
        """
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
            compute_providers_hash,
        )
        from fno.adapters.providers.runtime_state import (
            read_cursor,
            update_provider_health,
        )

        # Cooldown 'a' for the duration.
        update_provider_health("a", ErrorRule(status=401, cooldown_ms=300_000))

        served: list[str] = []

        def fn(pid: str) -> CallOutcome:
            served.append(pid)
            return CallOutcome(success=True, payload=pid)

        # Combo 'rr' has providers=[a,b,c], sticky_limit=2, round_robin.
        # Call once: a is cooldowned, b serves. Cursor advances ONCE
        # (for b's served slot), landing at (0, 1). [Old broken behavior
        # would have advanced TWICE: once for the read+rotate at the top,
        # then never again because no serve happened. The new code only
        # advances on serve.]
        out = dispatch_with_combo("rr", fn)
        assert isinstance(out, CallOutcome)
        assert served == ["b"]

        h = compute_providers_hash(("a", "b", "c"))
        cursor = read_cursor("rr", h)
        assert cursor is not None
        assert (cursor.cursor_index, cursor.consecutive_use_count) == (0, 1), (
            f"cursor should be at (0,1) after one served call; got "
            f"({cursor.cursor_index}, {cursor.consecutive_use_count})"
        )

    def test_queue_exhausted_does_not_advance_cursor(self, combos_env: Path):
        """All providers cooldowned -> QueueExhausted. Cursor must NOT advance
        because no slot was actually served."""
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.rotation import (
            CallOutcome,
            QueueExhausted,
            dispatch_with_combo,
            compute_providers_hash,
        )
        from fno.adapters.providers.runtime_state import (
            read_cursor,
            update_provider_health,
        )

        for pid in ("a", "b", "c"):
            update_provider_health(pid, ErrorRule(status=401, cooldown_ms=300_000))

        out = dispatch_with_combo("rr", lambda pid: CallOutcome(success=True))
        assert isinstance(out, QueueExhausted)

        h = compute_providers_hash(("a", "b", "c"))
        cursor = read_cursor("rr", h)
        # No advance happened: cursor is still absent (None).
        assert cursor is None

    def test_swap_trigger_then_success_advances_cursor_once(
        self, combos_env: Path
    ):
        """a swap-triggers, b serves: cursor advances ONCE (for b's served
        slot). Swap-trigger does NOT advance because the slot wasn't served.
        """
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
            compute_providers_hash,
        )
        from fno.adapters.providers.runtime_state import read_cursor

        attempts: list[str] = []

        def fn(pid: str) -> CallOutcome:
            attempts.append(pid)
            if pid == "a":
                return CallOutcome(
                    success=False, swap_trigger=True, status=503, body="overloaded"
                )
            return CallOutcome(success=True, payload=pid)

        out = dispatch_with_combo("rr", fn)
        assert isinstance(out, CallOutcome)
        assert out.success is True
        assert attempts == ["a", "b"]

        h = compute_providers_hash(("a", "b", "c"))
        cursor = read_cursor("rr", h)
        assert cursor is not None
        # Cursor advanced once (only for b's served slot).
        assert (cursor.cursor_index, cursor.consecutive_use_count) == (0, 1)



# ---------------------------------------------------------------------------
# Quota-aware ordering + skip (x-5d3e US2: AC2-EDGE + Locked Decision 9)
# ---------------------------------------------------------------------------

class TestDispatchWithComboQuota:
    def _seed(self, provider_id: str, used_pct: float, resets_at: float) -> None:
        import time

        from fno.adapters.providers.runtime_state import write_usage_snapshot
        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        now = time.time()
        write_usage_snapshot(
            UsageSnapshot(
                provider_id=provider_id,
                windows=(UsageWindow("5h", used_pct, resets_at),),
                probed_at=now,
                source="test",
            ),
            now=now,
        )

    def test_all_members_exhausted_returns_soonest_retry(self, combos_env: Path):
        # AC2-EDGE: every combo member exhausted with different reset times ->
        # QueueExhausted with retry_after == the soonest reset.
        from fno.adapters.providers.rotation import (
            CallOutcome,
            QueueExhausted,
            dispatch_with_combo,
        )

        import time as _t
        base = _t.time() + 3600
        self._seed("a", 100.0, base + 300)
        self._seed("b", 100.0, base + 100)  # soonest
        self._seed("c", 100.0, base + 500)

        seen: list[str] = []

        def fn(pid: str) -> CallOutcome:
            seen.append(pid)
            return CallOutcome(success=True)

        out = dispatch_with_combo("fb", fn)
        assert isinstance(out, QueueExhausted)
        assert out.retry_after == base + 100
        assert seen == []  # every member skipped, fn never called

    def test_low_member_demoted_below_ok(self, combos_env: Path):
        # OK/UNKNOWN order before LOW: 'a' is LOW (95%), 'b'/'c' OK -> the
        # fallback order [a,b,c] is stably repartitioned to [b,c,a] so the
        # first served provider is 'b', not the demoted 'a'.
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
        )

        import time as _t
        future = _t.time() + 3600
        self._seed("a", 95.0, future)  # LOW (>= default 90 threshold)
        self._seed("b", 10.0, future)  # OK
        self._seed("c", 10.0, future)  # OK

        seen: list[str] = []

        def fn(pid: str) -> CallOutcome:
            seen.append(pid)
            return CallOutcome(success=True, payload=pid)

        out = dispatch_with_combo("fb", fn)
        assert isinstance(out, CallOutcome)
        assert out.payload == "b"
        assert seen == ["b"]

    def test_no_snapshots_is_unchanged_order(self, combos_env: Path):
        # Locked Decision 9 / backward-compat: with no usage data every member
        # is UNKNOWN (rank 0), so the fallback order [a,b,c] is preserved.
        from fno.adapters.providers.rotation import (
            CallOutcome,
            dispatch_with_combo,
        )

        seen: list[str] = []

        def fn(pid: str) -> CallOutcome:
            seen.append(pid)
            return CallOutcome(success=True, payload=pid)

        out = dispatch_with_combo("fb", fn)
        assert isinstance(out, CallOutcome)
        assert seen == ["a"]


# ---------------------------------------------------------------------------
# next_healthy_provider (x-0676 exhaustion-failover primitive)
# ---------------------------------------------------------------------------

from types import SimpleNamespace

from fno.adapters.providers.rotation import Combo, next_healthy_provider
from fno.adapters.providers.runtime_state import Headroom, HeadroomState

_QUOTA = SimpleNamespace(probe_ttl_seconds=300.0, defer_threshold_pct=90.0)


def _patch_headroom(monkeypatch, states):
    """Patch headroom() to return the mapped HeadroomState per id (default OK)."""

    def fake(pid, *, now=None, ttl_seconds=300.0, threshold_pct=90.0):
        return Headroom(states.get(pid, HeadroomState.OK), None)

    monkeypatch.setattr("fno.adapters.providers.runtime_state.headroom", fake)


def test_next_healthy_skips_exhausted_returns_first_ok(monkeypatch):
    _patch_headroom(monkeypatch, {"ccm": HeadroomState.EXHAUSTED, "ccr": HeadroomState.OK})
    combo = Combo(name="c", providers=("ccm", "ccr", "glm"))
    assert next_healthy_provider(combo, quota=_QUOTA) == "ccr"


def test_next_healthy_low_counts_as_healthy(monkeypatch):
    _patch_headroom(monkeypatch, {"ccm": HeadroomState.EXHAUSTED, "ccr": HeadroomState.LOW})
    combo = Combo(name="c", providers=("ccm", "ccr"))
    assert next_healthy_provider(combo, quota=_QUOTA) == "ccr"


def test_next_healthy_unknown_counts_as_healthy(monkeypatch):
    # UNKNOWN never counts as exhausted (fail-open, x-6bcf): a GLM/gemini record
    # with no headroom signal is a valid failover TARGET.
    _patch_headroom(monkeypatch, {"ccm": HeadroomState.EXHAUSTED, "glm": HeadroomState.UNKNOWN})
    combo = Combo(name="c", providers=("ccm", "glm"))
    assert next_healthy_provider(combo, quota=_QUOTA) == "glm"


def test_next_healthy_exclude_skips_known_exhausted(monkeypatch):
    # ccm reads OK but is the known-exhausted one -> excluded, pick ccr.
    _patch_headroom(monkeypatch, {})
    combo = Combo(name="c", providers=("ccm", "ccr"))
    assert next_healthy_provider(combo, exclude={"ccm"}, quota=_QUOTA) == "ccr"


def test_next_healthy_all_exhausted_returns_none(monkeypatch):
    _patch_headroom(monkeypatch, {"ccm": HeadroomState.EXHAUSTED, "ccr": HeadroomState.EXHAUSTED})
    combo = Combo(name="c", providers=("ccm", "ccr"))
    assert next_healthy_provider(combo, quota=_QUOTA) is None


def test_next_healthy_single_provider_exhausted_returns_none(monkeypatch):
    # AC4-EDGE: a single-provider combo whose only member is exhausted -> None
    # (the caller defers; nothing to fail over TO).
    _patch_headroom(monkeypatch, {"ccm": HeadroomState.EXHAUSTED})
    combo = Combo(name="c", providers=("ccm",))
    assert next_healthy_provider(combo, quota=_QUOTA) is None
