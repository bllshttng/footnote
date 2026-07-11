"""Tests for quota-aware dispatch: usage probe, snapshot storage, headroom.

Run: cd cli && uv run pytest src/fno/adapters/providers/test_usage.py -v

Quota-aware dispatch (x-5d3e). Covers the probe fail-open contract, the
additive snapshot storage carried through the shared lock, and the headroom
predicate the routing/scheduling consumers act on.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.adapters.providers.error_taxonomy import ErrorRule
from fno.adapters.providers.model import ProviderRecord
from fno.adapters.providers.runtime_state import (
    Headroom,
    HeadroomState,
    headroom,
    read_state,
    read_usage,
    update_provider_health,
    write_usage_snapshot,
)
from fno.adapters.providers.usage import (
    UsageSnapshot,
    UsageWindow,
    probe_usage,
)


@pytest.fixture
def state_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    target = tmp_path / "provider-runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(target))
    return target


def _claude_record(creds: Path) -> ProviderRecord:
    return ProviderRecord(
        id="claude-primary",
        name="Claude Primary",
        cli="claude",
        auth="oauth_dir",
        credentials_source=creds,
    )


def _snap(provider_id: str, *windows: UsageWindow, probed_at: float = 1000.0) -> UsageSnapshot:
    return UsageSnapshot(
        provider_id=provider_id,
        windows=tuple(windows),
        probed_at=probed_at,
        source="test",
    )


# ---------------------------------------------------------------------------
# UsageWindow clamp invariant (Boundaries: 0, 100, >100, <0)
# ---------------------------------------------------------------------------


class TestUsageWindowClamp:
    def test_in_range_preserved(self) -> None:
        assert UsageWindow("5h", 42.0, 2000.0).used_pct == 42.0

    def test_zero_and_hundred_exact(self) -> None:
        assert UsageWindow("5h", 0.0, 2000.0).used_pct == 0.0
        assert UsageWindow("5h", 100.0, 2000.0).used_pct == 100.0

    def test_over_hundred_clamped(self) -> None:
        assert UsageWindow("5h", 103.0, 2000.0).used_pct == 100.0

    def test_negative_clamped(self) -> None:
        assert UsageWindow("5h", -5.0, 2000.0).used_pct == 0.0


# ---------------------------------------------------------------------------
# probe_usage fail-open + crash containment (AC1-ERR, AC1-FR)
# ---------------------------------------------------------------------------


class TestProbeFailOpen:
    def test_api_key_record_is_unknown(self) -> None:
        rec = ProviderRecord(
            id="api", name="Api", cli="claude", auth="api_key",
            env={"ANTHROPIC_API_KEY": "x"},
        )
        assert probe_usage(rec) is None

    def test_unknown_cli_is_unknown(self, tmp_path: Path) -> None:
        rec = ProviderRecord(
            id="gem", name="Gem", cli="gemini", auth="oauth_dir",
            credentials_source=tmp_path,
        )
        assert probe_usage(rec) is None

    def test_missing_credentials_is_unknown(self, tmp_path: Path) -> None:
        # No .credentials.json in the dir -> bearer read fails -> None, no raise.
        assert probe_usage(_claude_record(tmp_path)) is None

    def test_probe_crash_is_contained(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC1-FR: an unexpected exception inside a per-CLI probe is caught at
        # the probe_usage boundary and mapped to None, never propagated.
        import fno.adapters.providers.usage as usage_mod

        def boom(record: ProviderRecord, now: float) -> UsageSnapshot | None:
            raise RuntimeError("endpoint exploded")

        monkeypatch.setitem(usage_mod._PROBES, "claude", boom)
        assert probe_usage(_claude_record(tmp_path)) is None

    def test_claude_probe_parses_windows(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC1-HP shape: a well-formed endpoint payload becomes a snapshot.
        (tmp_path / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "tok"}})
        )
        import fno.adapters.providers.usage as usage_mod

        class _Resp:
            def __enter__(self):  # noqa: ANN001
                return self

            def __exit__(self, *a):  # noqa: ANN001
                return False

            def read(self):  # noqa: ANN001
                return json.dumps(
                    {"windows": [{"label": "5h", "used_pct": 87.5, "resets_at": 5000.0}]}
                ).encode()

        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", lambda *a, **k: _Resp())
        snap = probe_usage(_claude_record(tmp_path), now=1000.0)
        assert snap is not None
        assert snap.provider_id == "claude-primary"
        assert snap.windows[0].label == "5h"
        assert snap.windows[0].used_pct == 87.5


# ---------------------------------------------------------------------------
# Snapshot storage: round-trip, TTL, carry-through under the shared lock
# ---------------------------------------------------------------------------


class TestSnapshotStorage:
    def test_write_read_roundtrip(self, state_path: Path) -> None:
        snap = _snap("p1", UsageWindow("5h", 50.0, 9000.0), probed_at=1000.0)
        write_usage_snapshot(snap, now=1000.0)
        got = read_usage("p1", ttl_seconds=300, now=1100.0)
        assert got is not None
        assert got.windows[0].used_pct == 50.0
        # AC1-HP: persisted under `usage` in the state file.
        raw = json.loads(state_path.read_text())
        assert "p1" in raw["usage"]

    def test_stale_snapshot_reads_as_absent(self, state_path: Path) -> None:
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 50.0, 9000.0), probed_at=1000.0))
        # 400s later with a 300s TTL -> treated as absent.
        assert read_usage("p1", ttl_seconds=300, now=1400.0) is None

    def test_health_write_preserves_usage(self, state_path: Path) -> None:
        # Concurrency invariant: a health mutation must not drop the usage
        # field written under the same lock (and vice versa).
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 50.0, 9000.0), probed_at=1000.0), now=1000.0)
        update_provider_health("p1", ErrorRule(status=429, backoff=True), now=1001.0)
        assert read_usage("p1", ttl_seconds=300, now=1002.0) is not None
        # And the health write landed too.
        assert read_state(now=1002.0).provider_health["p1"].backoff_level == 1

    def test_malformed_entry_self_heals(self, state_path: Path) -> None:
        # AC2-ERR: a hand-corrupted usage entry (string used_pct, missing
        # resets_at) is dropped on read, not raised.
        state_path.write_text(
            json.dumps(
                {
                    "schema_version": 2,
                    "provider_health": {},
                    "combo_cursors": {},
                    "usage": {
                        "bad": {
                            "provider_id": "bad",
                            "windows": [{"label": "5h", "used_pct": "oops"}],
                            "probed_at": 1000.0,
                            "source": "test",
                        }
                    },
                }
            )
        )
        # Read does not raise; the bad entry is gone.
        assert read_usage("bad", ttl_seconds=300, now=1000.0) is None
        assert read_state(now=1000.0).usage == {}


# ---------------------------------------------------------------------------
# Headroom predicate (AC1-EDGE + Locked Decision 9 ordering upstream)
# ---------------------------------------------------------------------------


class TestHeadroom:
    def test_unknown_when_no_snapshot(self, state_path: Path) -> None:
        assert headroom("p1", now=1000.0).state is HeadroomState.UNKNOWN

    def test_empty_windows_is_unknown_not_ok(self, state_path: Path) -> None:
        write_usage_snapshot(_snap("p1", probed_at=1000.0), now=1000.0)
        assert headroom("p1", now=1000.0).state is HeadroomState.UNKNOWN

    def test_exhausted_future_window(self, state_path: Path) -> None:
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 5000.0), probed_at=1000.0), now=1000.0)
        h = headroom("p1", now=1000.0)
        assert h.state is HeadroomState.EXHAUSTED
        assert h.resets_at == 5000.0

    def test_low_at_threshold(self, state_path: Path) -> None:
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 90.0, 5000.0), probed_at=1000.0), now=1000.0)
        assert headroom("p1", now=1000.0, threshold_pct=90.0).state is HeadroomState.LOW

    def test_ok_below_threshold(self, state_path: Path) -> None:
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 40.0, 5000.0), probed_at=1000.0), now=1000.0)
        assert headroom("p1", now=1000.0, threshold_pct=90.0).state is HeadroomState.OK

    def test_stale_exhaustion_never_binds(self, state_path: Path) -> None:
        # AC1-EDGE: a 100% window whose resets_at is in the past does not bind;
        # the limit has reset, so dispatch proceeds (OK).
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 500.0), probed_at=1000.0), now=1000.0)
        assert headroom("p1", now=1000.0).state is HeadroomState.OK

    def test_provider_rate_limited_until_is_exhausted(self, state_path: Path) -> None:
        # An active provider-level rate_limited_until reads EXHAUSTED even
        # without a usage snapshot.
        update_provider_health("p1", ErrorRule(status=429, cooldown_ms=60_000), now=1000.0)
        h = headroom("p1", now=1000.0)
        assert h.state is HeadroomState.EXHAUSTED
        assert h.resets_at is not None and h.resets_at > 1000.0
