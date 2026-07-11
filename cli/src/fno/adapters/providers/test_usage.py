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

from fno.adapters.providers import loader
from fno.adapters.providers.error_taxonomy import ErrorRule
from fno.adapters.providers.model import ProviderRecord
from fno.adapters.providers.runtime_state import (
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


@pytest.fixture(autouse=True)
def _isolate_keychain(monkeypatch: pytest.MonkeyPatch) -> None:
    """Never touch the real macOS Keychain in tests (would leak a dev's token).

    Default to 'no keychain blobs'; a test that wants a Keychain token opts in
    by re-patching _read_claude_keychain_blobs.
    """
    import fno.adapters.providers.usage as usage_mod

    monkeypatch.setattr(usage_mod, "_read_claude_keychain_blobs", lambda cfg: [])


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

    def test_claude_probe_parses_real_shape(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Verified /api/oauth/usage shape (x-6bcf): top-level five_hour/seven_day
        # objects with utilization (0-100) + an ISO-8601 resets_at string.
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
                return json.dumps({
                    "five_hour": {"utilization": 9.0, "resets_at": "2026-07-12T02:09:59+00:00"},
                    "seven_day": {"utilization": 69.0, "resets_at": "2026-07-12T10:59:59+00:00"},
                    "seven_day_opus": None,
                }).encode()

        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", lambda *a, **k: _Resp())
        snap = probe_usage(_claude_record(tmp_path), now=1000.0)
        assert snap is not None
        labels = {w.label: w.used_pct for w in snap.windows}
        assert labels == {"5h": 9.0, "weekly": 69.0}
        # resets_at parsed from ISO to epoch.
        import datetime as _dt
        assert snap.windows[0].resets_at == _dt.datetime.fromisoformat("2026-07-12T02:09:59+00:00").timestamp()

    def test_claude_probe_skips_stale_token_then_succeeds(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A stale scoped Keychain token 401s; the probe tries the next candidate.
        import urllib.error

        import fno.adapters.providers.usage as usage_mod

        monkeypatch.setattr(
            usage_mod, "_read_claude_keychain_blobs",
            lambda cfg: [
                json.dumps({"claudeAiOauth": {"accessToken": "stale"}}),
                json.dumps({"claudeAiOauth": {"accessToken": "live"}}),
            ],
        )

        class _Resp:
            def __enter__(self):  # noqa: ANN001
                return self

            def __exit__(self, *a):  # noqa: ANN001
                return False

            def read(self):  # noqa: ANN001
                return json.dumps({"five_hour": {"utilization": 5.0, "resets_at": "2026-07-12T02:00:00+00:00"}}).encode()

        def _fetch(req, timeout):  # noqa: ANN001
            if "stale" in req.headers.get("Authorization", ""):
                raise urllib.error.HTTPError(req.full_url, 401, "Unauthorized", {}, None)
            return _Resp()

        # No file token (empty dir) so only the two keychain tokens are tried.
        rec = ProviderRecord(id="c", name="c", cli="claude", auth="oauth_dir", credentials_source=tmp_path)
        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", _fetch)
        snap = probe_usage(rec, now=1000.0)
        assert snap is not None
        assert snap.windows[0].used_pct == 5.0

    def test_codex_probe_parses_real_shape(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Verified codex shape (x-6bcf): an event_msg line with rate_limits at
        # payload.rate_limits; each window has used_percent + an ABSOLUTE resets_at.
        sessions = tmp_path / "sessions"
        sessions.mkdir()
        (sessions / "s.jsonl").write_text(json.dumps({
            "timestamp": "2026-07-11T14:11:44",
            "type": "event_msg",
            "payload": {"rate_limits": {
                "primary": {"used_percent": 4.0, "window_minutes": 300, "resets_at": 1783807404},
                "secondary": {"used_percent": 5.0, "window_minutes": 10080, "resets_at": 1784372823},
            }},
        }) + "\n")
        rec = ProviderRecord(id="cx", name="cx", cli="codex", auth="oauth_dir", credentials_source=tmp_path)
        snap = probe_usage(rec, now=1000.0)
        assert snap is not None
        assert snap.source == "session-events"
        got = {w.label: (w.used_pct, w.resets_at) for w in snap.windows}
        assert got == {"5h": (4.0, 1783807404.0), "weekly": (5.0, 1784372823.0)}


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


# ---------------------------------------------------------------------------
# evaluate_quota_defer: the dispatcher decision core (US3: AC2-HP, AC2-FR, LD)
# ---------------------------------------------------------------------------


class TestEvaluateQuotaDefer:
    def _quota(self, monkeypatch, **kw) -> None:
        from fno.adapters.providers.model import QuotaConfig

        cfg = QuotaConfig(**kw)
        monkeypatch.setattr(loader, "load_quota_config", lambda *a, **k: cfg)

    def test_off_by_default_never_defers(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_defer

        self._quota(monkeypatch, defer_dispatch=False)
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=1000.0), now=1000.0)
        assert evaluate_quota_defer("p1", priority="p2", now=1000.0) is None

    def test_p0_never_defers(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_defer

        self._quota(monkeypatch, defer_dispatch=True)
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=1000.0), now=1000.0)
        assert evaluate_quota_defer("p1", priority="p0", now=1000.0) is None

    def test_exhausted_defers_with_retry_at(self, state_path: Path, monkeypatch) -> None:
        # AC2-HP core: exhausted -> defer, retry_at == the window reset.
        from fno.adapters.providers.runtime_state import HeadroomState, evaluate_quota_defer

        self._quota(monkeypatch, defer_dispatch=True)
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=1000.0), now=1000.0)
        d = evaluate_quota_defer("p1", priority="p2", now=1000.0)
        assert d is not None
        assert d.state is HeadroomState.EXHAUSTED
        assert d.retry_at == 9e18

    def test_low_within_horizon_defers(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_defer

        self._quota(monkeypatch, defer_dispatch=True, defer_horizon_minutes=60, defer_threshold_pct=90.0)
        # reset in 30 min (< 60 horizon), 95% -> LOW -> defer.
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 95.0, 1000.0 + 1800), probed_at=1000.0), now=1000.0)
        assert evaluate_quota_defer("p1", priority="p2", now=1000.0) is not None

    def test_low_outside_horizon_proceeds(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_defer

        self._quota(monkeypatch, defer_dispatch=True, defer_horizon_minutes=60)
        # reset in 2h (> 60 horizon), 95% -> LOW but too far -> proceed.
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 95.0, 1000.0 + 7200), probed_at=1000.0), now=1000.0)
        assert evaluate_quota_defer("p1", priority="p2", now=1000.0) is None

    def test_unknown_never_strands(self, state_path: Path, monkeypatch) -> None:
        # AC2-FR: a deferred node whose snapshot ages out degrades to UNKNOWN,
        # which never defers -> the next tick dispatches (deferral cannot outlive
        # the evidence). No fresh snapshot -> UNKNOWN -> None. refresh_usage will
        # try to probe; with no provider record it returns None, staying UNKNOWN.
        from fno.adapters.providers.model import ProvidersConfig
        from fno.adapters.providers.runtime_state import evaluate_quota_defer

        self._quota(monkeypatch, defer_dispatch=True)
        monkeypatch.setattr(loader, "load_providers", lambda *a, **k: ProvidersConfig(records=[]))
        assert evaluate_quota_defer("p1", priority="p2", now=1000.0) is None


class TestDispatchOneQuotaDefer:
    def test_default_selection_defers_and_emits(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC2-HP + AC1-UI: the default-selection dispatcher tick defers an
        # exhausted node with a visible receipt AND one decision event.
        import json as _json

        from fno.adapters.providers.model import QuotaConfig
        import fno.dispatch as dispatch_mod

        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "rt.json"))
        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(
            loader, "load_quota_config", lambda *a, **k: QuotaConfig(defer_dispatch=True)
        )
        monkeypatch.setattr(dispatch_mod, "_resolve_provider_id", lambda: "p1")
        monkeypatch.setattr(
            dispatch_mod, "_next_node", lambda project: {"id": "ab-9f", "slug": "x", "priority": "p2"}
        )
        import time as _t
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=_t.time()))

        verdict = dispatch_mod._dispatch_one(session="s", node=None, project=None)
        assert verdict["outcome"] == "quota-deferred"
        assert verdict["node"] == "ab-9f"
        assert verdict["provider"] == "p1"
        assert verdict["retry_at"] == 9e18
        # One decision event landed.
        events = (tmp_path / ".fno" / "events.jsonl").read_text().splitlines()
        rows = [_json.loads(ln) for ln in events if ln.strip()]
        deferred = [r for r in rows if r["type"] == "quota_deferred"]
        assert len(deferred) == 1
        assert deferred[0]["data"]["provider"] == "p1"

    def test_explicit_node_never_defers(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # LD#5: an explicit --node dispatch is a human verb and always fires,
        # so it must not even consult quota. Proven by making the dispatch reach
        # the spawn boundary (patched to a sentinel outcome).
        from fno.adapters.providers.model import QuotaConfig
        import fno.dispatch as dispatch_mod

        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "rt.json"))
        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(
            loader, "load_quota_config", lambda *a, **k: QuotaConfig(defer_dispatch=True)
        )
        monkeypatch.setattr(dispatch_mod, "_lookup_node", lambda n: {"id": n, "slug": "x", "priority": "p2"})
        # Exhausted snapshot present, but explicit path must ignore it.
        import time as _t
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=_t.time()))

        # Force the downstream claim path to short-circuit so we only assert we
        # did NOT quota-defer: a live dispatch reservation yields already-dispatching.
        monkeypatch.setattr(dispatch_mod, "_claim_is_live", lambda key: True)
        verdict = dispatch_mod._dispatch_one(session="s", node="ab-77", project=None)
        assert verdict["outcome"] != "quota-deferred"


# ---------------------------------------------------------------------------
# Required-bot promise-time exhaustion warning (US5: AC3-HP)
# ---------------------------------------------------------------------------


class TestRequiredBotHeadroomCheck:
    def test_exhausted_required_bot_warns_and_emits(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import json as _json
        import time as _t
        from types import SimpleNamespace

        from fno.adapters.providers import cli as pcli
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "rt.json"))
        monkeypatch.chdir(tmp_path)
        # Config: one required bot backed by codex.
        review = SimpleNamespace(github_apps=["chatgpt-codex-connector"], required_bots=None)
        monkeypatch.setattr("fno.config.load_settings", lambda *a, **k: SimpleNamespace(review=review))
        rec = ProviderRecord(id="codex-pro", name="Codex", cli="codex", auth="api_key", env={"OPENAI_API_KEY": "x"})
        monkeypatch.setattr(pcli, "load_providers", lambda *a, **k: ProvidersConfig(records=[rec]))
        monkeypatch.setattr(pcli, "_get_repo_root", lambda: tmp_path)

        now = _t.time()
        write_usage_snapshot(_snap("codex-pro", UsageWindow("5h", 100.0, now + 3600), probed_at=now), now=now)

        warnings = pcli.required_bot_headroom_check()
        assert len(warnings) == 1
        assert warnings[0]["bot"] == "chatgpt-codex-connector"
        assert warnings[0]["provider"] == "codex-pro"
        # AC3-HP: one decision event emitted naming bot + provider + reset.
        rows = [
            _json.loads(ln)
            for ln in (tmp_path / ".fno" / "events.jsonl").read_text().splitlines()
            if ln.strip()
        ]
        ev = [r for r in rows if r["type"] == "quota_required_bot_exhausted"]
        assert len(ev) == 1
        assert ev[0]["data"]["bot"] == "chatgpt-codex-connector"
        assert ev[0]["data"]["retry_at"] == now + 3600

    def test_healthy_required_bot_is_quiet(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import time as _t
        from types import SimpleNamespace

        from fno.adapters.providers import cli as pcli
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "rt.json"))
        monkeypatch.chdir(tmp_path)
        review = SimpleNamespace(github_apps=["chatgpt-codex-connector"], required_bots=None)
        monkeypatch.setattr("fno.config.load_settings", lambda *a, **k: SimpleNamespace(review=review))
        rec = ProviderRecord(id="codex-pro", name="Codex", cli="codex", auth="api_key", env={"OPENAI_API_KEY": "x"})
        monkeypatch.setattr(pcli, "load_providers", lambda *a, **k: ProvidersConfig(records=[rec]))
        monkeypatch.setattr(pcli, "_get_repo_root", lambda: tmp_path)

        now = _t.time()
        write_usage_snapshot(_snap("codex-pro", UsageWindow("5h", 20.0, now + 3600), probed_at=now), now=now)
        assert pcli.required_bot_headroom_check() == []
