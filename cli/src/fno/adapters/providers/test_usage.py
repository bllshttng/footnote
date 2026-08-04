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
    probe_usage_detail,
)

# Captured at import time, before the autouse keychain stub replaces the module
# attribute: the one test that exercises the Keychain lookup itself needs the
# real function, not the stub.
from fno.adapters.providers.usage import (  # noqa: E402  isort:skip
    _read_claude_keychain_blobs as _real_keychain_read,
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
        harness="claude",
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
            id="api", name="Api", harness="claude", auth="api_key",
            env={"ANTHROPIC_API_KEY": "x"},
        )
        assert probe_usage(rec) is None

    def test_unknown_cli_is_unknown(self, tmp_path: Path) -> None:
        rec = ProviderRecord(
            id="gem", name="Gem", harness="gemini", auth="oauth_dir",
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

    def test_claude_probe_includes_model_weekly_windows(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # x-6bcf review: a populated seven_day_opus must NOT be dropped - a maxed
        # Opus weekly has to bind headroom even when the general weekly has room.
        # Obfuscated promo buckets (tangelo) are excluded.
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
                    "five_hour": {"utilization": 5.0, "resets_at": "2026-07-12T02:00:00+00:00"},
                    "seven_day": {"utilization": 40.0, "resets_at": "2026-07-12T10:00:00+00:00"},
                    "seven_day_opus": {"utilization": 100.0, "resets_at": "2026-07-12T11:00:00+00:00"},
                    "seven_day_sonnet": None,
                    "tangelo": {"utilization": 99.0, "resets_at": "2026-07-12T12:00:00+00:00"},
                }).encode()

        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", lambda *a, **k: _Resp())
        snap = probe_usage(_claude_record(tmp_path), now=1000.0)
        assert snap is not None
        labels = {w.label: w.used_pct for w in snap.windows}
        # opus included (as weekly-opus); sonnet null skipped; tangelo excluded.
        assert labels == {"5h": 5.0, "weekly": 40.0, "weekly-opus": 100.0}
        # The maxed opus window binds: headroom would read it as the worst.
        assert max(w.used_pct for w in snap.windows) == 100.0

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
        rec = ProviderRecord(id="c", name="c", harness="claude", auth="oauth_dir", credentials_source=tmp_path)
        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", _fetch)
        snap = probe_usage(rec, now=1000.0)
        assert snap is not None
        assert snap.windows[0].used_pct == 5.0

    def test_config_dir_record_reads_only_its_own_scoped_item(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC6-HP: a config_dir record probes THAT dir's credential. The scoped
        # Keychain lookup is keyed on the dir, so two config_dir accounts read
        # independently rather than both returning the shared slot's numbers.
        import fno.adapters.providers.usage as usage_mod

        alt = tmp_path / "claude-alt"
        alt.mkdir()
        (alt / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "alt-token"}})
        )
        rec = ProviderRecord(
            id="alt", name="alt", harness="claude", auth="managed", config_dir=alt
        )
        assert usage_mod._claude_bearer_candidates(rec) == ["alt-token"]

    def test_own_dir_never_falls_back_to_the_unscoped_item(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Evidence 2b: the unscoped Keychain item belongs to whoever occupies the
        # shared ~/.claude slot. A record with its own dir must never borrow it -
        # that is how a per-account probe reports the ACTIVE account's usage.
        import fno.adapters.providers.usage as usage_mod

        seen: list[str] = []

        def _fake_security(args: list[str], **kwargs: object):  # noqa: ANN001
            seen.append(args[args.index("-s") + 1])

            class _Out:
                returncode = 1
                stdout = ""

            return _Out()

        monkeypatch.setattr(usage_mod.sys, "platform", "darwin")
        monkeypatch.setattr(usage_mod.subprocess, "run", _fake_security)
        _real_keychain_read(tmp_path)
        assert len(seen) == 1
        assert seen[0].startswith("Claude Code-credentials-")

    def test_managed_active_slot_occupant_is_probeable(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC1-HP: the whole defect. `auth: managed` used to short-circuit to None
        # before any bearer lookup, so a live account read `unknown` forever.
        import fno.adapters.providers.usage as usage_mod

        # The slot is the CANONICAL ~/.claude, so HOME is what moves it - an
        # ambient CLAUDE_CONFIG_DIR must NOT, or a worker pinned elsewhere would
        # make this probe read its credential.
        monkeypatch.setenv("HOME", str(tmp_path))
        slot = tmp_path / ".claude"
        slot.mkdir()
        (slot / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "slot-token"}})
        )
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda rec: True)
        rec = ProviderRecord(id="primary", name="primary", harness="claude", auth="managed")
        assert usage_mod._claude_bearer_candidates(rec) == ["slot-token"]

    def test_managed_non_occupant_refuses_rather_than_borrowing(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # AC2-ERR: a managed record that is NOT the slot occupant has no
        # attributable credential, so it probes nothing and no snapshot is ever
        # written under its id carrying another account's numbers.
        import fno.adapters.providers.usage as usage_mod

        monkeypatch.setenv("HOME", str(tmp_path))
        slot = tmp_path / ".claude"
        slot.mkdir()
        (slot / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "slot-token"}})
        )
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda rec: False)
        rec = ProviderRecord(id="other", name="other", harness="claude", auth="managed")
        assert usage_mod._claude_bearer_candidates(rec) == []
        assert probe_usage(rec) is None

    def test_an_ambient_config_dir_never_redirects_the_slot_read(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A worker pinned to account B runs with B's dir exported. Honoring that
        # here would read B's credential while the stamp names A, and file B's
        # usage under A - the exact lie the attribution rule exists to prevent.
        import fno.adapters.providers.usage as usage_mod

        monkeypatch.setenv("HOME", str(tmp_path))
        slot = tmp_path / ".claude"
        slot.mkdir()
        (slot / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "account-a"}})
        )
        other = tmp_path / "claude-b"
        other.mkdir()
        (other / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "account-b"}})
        )
        monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(other))
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda rec: True)
        rec = ProviderRecord(id="a", name="a", harness="claude", auth="managed")
        assert usage_mod._claude_bearer_candidates(rec) == ["account-a"]

    def test_managed_store_blob_is_not_a_probe_source(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # The blob is a capture-time copy: it goes stale and it can duplicate
        # across ids (Evidence 2). Reading it would report a dead token's window
        # or another account's usage, so the probe must not reach for it.
        import fno.adapters.providers.usage as usage_mod

        root = tmp_path / "providers"
        (root / "other").mkdir(parents=True)
        (root / "other" / "blob").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "blob-token"}})
        )
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda rec: False)
        rec = ProviderRecord(id="other", name="other", harness="claude", auth="managed")
        assert usage_mod._claude_bearer_candidates(rec) == []

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
        rec = ProviderRecord(id="cx", name="cx", harness="codex", auth="oauth_dir", credentials_source=tmp_path)
        snap = probe_usage(rec, now=1000.0)
        assert snap is not None
        assert snap.source == "session-events"
        got = {w.label: (w.used_pct, w.resets_at) for w in snap.windows}
        assert got == {"5h": (4.0, 1783807404.0), "weekly": (5.0, 1784372823.0)}


# ---------------------------------------------------------------------------
# Snapshot storage: round-trip, TTL, carry-through under the shared lock
# ---------------------------------------------------------------------------


def test_iso_to_epoch_handles_z_suffix() -> None:
    # gemini review: Py<3.11 fromisoformat rejects a trailing 'Z'.
    from datetime import datetime, timezone

    from fno.adapters.providers.usage import _iso_to_epoch

    expected = datetime(2026, 7, 12, 2, 0, 0, tzinfo=timezone.utc).timestamp()
    assert _iso_to_epoch("2026-07-12T02:00:00Z") == expected
    assert _iso_to_epoch("2026-07-12T02:00:00+00:00") == expected
    assert _iso_to_epoch(1234567890) == 1234567890.0
    assert _iso_to_epoch("garbage") is None
    assert _iso_to_epoch(None) is None


# ---------------------------------------------------------------------------
# Typed refresh observation: a successful probe survives a failed cache
# write, and every unknown names the boundary that produced it.
# ---------------------------------------------------------------------------


class TestProbeUnknownReason:
    """AC3-ERR: `None` is one value with four causes; name them."""

    def test_unattributed_record(self) -> None:
        """A managed record that is not its CLI's slot occupant."""
        rec = ProviderRecord(id="m", name="M", harness="claude", auth="managed")
        assert probe_usage_detail(rec) == (None, "unattributed")

    def test_harness_without_a_probe(self, tmp_path: Path) -> None:
        rec = ProviderRecord(
            id="g", name="G", harness="gemini", auth="oauth_dir",
            credentials_source=tmp_path,
        )
        assert probe_usage_detail(rec) == (None, "harness-unsupported")

    def test_api_key_record_is_an_auth_gap_not_an_attribution_gap(self) -> None:
        """An api_key record's credential IS its own; it just is not a bearer.

        Reporting `unattributed` here would send an operator to repair an
        account binding that is already correct.
        """
        rec = ProviderRecord(
            id="k", name="K", harness="claude", auth="api_key",
            env={"ANTHROPIC_API_KEY": "x"},
        )
        assert probe_usage_detail(rec) == (None, "auth-unsupported")

    def test_capability_is_classified_before_attribution(self) -> None:
        """An unsupported harness answers first, whatever its auth shape.

        Otherwise every gemini/openclaw api_key record - the common case - reads
        as an attribution failure that no amount of re-login can fix.
        """
        rec = ProviderRecord(
            id="g", name="G", harness="gemini", auth="api_key",
            env={"GEMINI_API_KEY": "x"},
        )
        assert probe_usage_detail(rec) == (None, "harness-unsupported")

    def test_probe_returning_none(self, tmp_path: Path) -> None:
        # No credential file and no keychain blob -> the claude probe runs and
        # reads nothing. Attribution succeeded, so this is NOT `unattributed`.
        assert probe_usage_detail(_claude_record(tmp_path)) == (None, "probe-failed")

    def test_probe_raising(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import fno.adapters.providers.usage as usage_mod

        def _boom(record: ProviderRecord, now: float) -> None:
            raise RuntimeError("endpoint drift")

        monkeypatch.setitem(usage_mod._PROBES, "claude", _boom)
        assert probe_usage_detail(_claude_record(tmp_path)) == (None, "probe-error")

    def test_a_rejected_bearer_is_attribution_not_a_probe_failure(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A credential the slot would not vouch for never reaches the endpoint.

        `_probe_claude` refuses every candidate whose bearer verdict is not
        `match`/`unsupported`, so NO usage request is issued. Reporting that as
        `probe-failed` sends an operator to debug a network path that was never
        used - a confident wrong reason, which is worse than a bare unknown.
        """
        import fno.adapters.providers.usage as usage_mod

        (tmp_path / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "tok"}})
        )
        rec = _claude_record(tmp_path)
        monkeypatch.setattr(usage_mod, "_bearer_verdict", lambda r, b, n: "mismatch")
        monkeypatch.setattr(usage_mod, "_reconcile_slot_once", lambda r, n: False)

        def _never(*a: object, **k: object) -> None:
            pytest.fail("a rejected bearer must never reach the usage endpoint")

        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", _never)
        assert probe_usage_detail(rec, now=1000.0) == (None, "unattributed")

    def test_probe_usage_delegates_to_the_detail_path(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """One probe implementation, not two: the wrapper must not fork.

        A guard or a stub on one of two reachable probe paths is decorative, so
        pin that `probe_usage` is exactly `probe_usage_detail(...)[0]`.
        """
        import fno.adapters.providers.usage as usage_mod

        snap = _snap("claude-primary", UsageWindow("5h", 7.0, 9000.0))
        monkeypatch.setitem(usage_mod._PROBES, "claude", lambda record, now: (snap, None))
        rec = _claude_record(tmp_path)
        assert probe_usage(rec) is probe_usage_detail(rec)[0] is snap


class TestRefreshObservation:
    def test_a_probed_snapshot_survives_a_failed_cache_write(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC2-FR: losing the write race is a cache outcome, not a probe outcome.

        Failure Modes / Errors: "a cache lock timeout reports persistence
        degradation without discarding a successful snapshot."
        """
        from fno.adapters.providers import runtime_state as rs

        snap = _snap("p1", UsageWindow("5h", 12.0, 9000.0))
        monkeypatch.setattr(rs, "write_usage_snapshot", lambda s, now=None: False)
        monkeypatch.setattr(
            "fno.adapters.providers.loader.load_providers",
            lambda *a, **k: type(
                "C", (), {"by_id": {"p1": _claude_record(state_path.parent)}}
            )(),
        )
        monkeypatch.setattr(
            "fno.adapters.providers.usage.probe_usage_detail",
            lambda record, now=None: (snap, None),
        )

        obs = rs.refresh_usage_detailed("p1", ttl_seconds=0, now=2000.0)
        assert obs.snapshot is snap
        assert obs.persisted is False
        assert obs.reason is None
        assert obs.known is True
        # The compatibility wrapper still yields the same snapshot.
        assert rs.refresh_usage("p1", ttl_seconds=0, now=2000.0) is snap

    def test_write_reports_success(self, state_path: Path) -> None:
        assert write_usage_snapshot(_snap("p1", UsageWindow("5h", 1.0, 9000.0))) is True

    def test_write_reports_lock_contention(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import filelock

        from fno.adapters.providers import runtime_state as rs

        class _AlwaysContended:
            def __init__(self, *a: object, **k: object) -> None: ...

            def __enter__(self) -> None:
                raise filelock.Timeout("busy")

            def __exit__(self, *a: object) -> None: ...

        monkeypatch.setattr(rs.filelock, "FileLock", _AlwaysContended)
        assert write_usage_snapshot(_snap("p1", UsageWindow("5h", 1.0, 9000.0))) is False

    def test_missing_record_is_named(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.adapters.providers import runtime_state as rs

        monkeypatch.setattr(
            "fno.adapters.providers.loader.load_providers",
            lambda *a, **k: type("C", (), {"by_id": {}})(),
        )
        obs = rs.refresh_usage_detailed("ghost", ttl_seconds=0, now=2000.0)
        assert (obs.snapshot, obs.reason, obs.known) == (None, "record-missing", False)

    def test_unreadable_config_is_named(
        self, state_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.adapters.providers import runtime_state as rs

        def _boom(*a: object, **k: object) -> None:
            raise OSError("config gone")

        monkeypatch.setattr("fno.adapters.providers.loader.load_providers", _boom)
        obs = rs.refresh_usage_detailed("p1", ttl_seconds=0, now=2000.0)
        assert obs.reason == "config-unreadable"

    def test_a_snapshot_with_no_windows_is_unknown_with_a_reason(
        self, state_path: Path
    ) -> None:
        from fno.adapters.providers import runtime_state as rs

        write_usage_snapshot(_snap("p1", probed_at=1000.0), now=1000.0)
        obs = rs.refresh_usage_detailed("p1", ttl_seconds=300, now=1100.0)
        assert obs.snapshot is not None
        assert (obs.reason, obs.known) == ("no-windows", False)

    def test_a_cache_hit_attempts_no_write(self, state_path: Path) -> None:
        from fno.adapters.providers import runtime_state as rs

        write_usage_snapshot(
            _snap("p1", UsageWindow("5h", 3.0, 9000.0), probed_at=1000.0), now=1000.0
        )
        obs = rs.refresh_usage_detailed("p1", ttl_seconds=300, now=1100.0)
        assert obs.known is True
        assert obs.persisted is None  # tri-state: no write was attempted


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
# evaluate_quota_signal: the dispatcher decision core (US3: AC2-HP, AC2-FR, LD)
# ---------------------------------------------------------------------------


class TestEvaluateQuotaDefer:
    def _quota(self, monkeypatch, **kw) -> None:
        from fno.adapters.providers.model import QuotaConfig

        cfg = QuotaConfig(**kw)
        monkeypatch.setattr(loader, "load_quota_config", lambda *a, **k: cfg)

    def test_off_by_default_never_defers(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_signal

        self._quota(monkeypatch, defer_dispatch=False)
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=1000.0), now=1000.0)
        assert not evaluate_quota_signal("p1", priority="p2", now=1000.0).defer

    def test_p0_never_defers(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_signal

        self._quota(monkeypatch, defer_dispatch=True)
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=1000.0), now=1000.0)
        assert not evaluate_quota_signal("p1", priority="p0", now=1000.0).defer

    def test_exhausted_defers_with_retry_at(self, state_path: Path, monkeypatch) -> None:
        # AC2-HP core: exhausted -> defer, resets_at == the window reset.
        from fno.adapters.providers.runtime_state import HeadroomState, evaluate_quota_signal

        self._quota(monkeypatch, defer_dispatch=True)
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 100.0, 9e18), probed_at=1000.0), now=1000.0)
        sig = evaluate_quota_signal("p1", priority="p2", now=1000.0)
        assert sig.defer
        assert sig.state is HeadroomState.EXHAUSTED
        assert sig.resets_at == 9e18

    def test_low_within_horizon_defers(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_signal

        self._quota(monkeypatch, defer_dispatch=True, defer_horizon_minutes=60, defer_threshold_pct=90.0)
        # reset in 30 min (< 60 horizon), 95% -> LOW -> defer.
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 95.0, 1000.0 + 1800), probed_at=1000.0), now=1000.0)
        assert evaluate_quota_signal("p1", priority="p2", now=1000.0).defer

    def test_defer_wins_when_the_two_windows_overlap(self, state_path: Path, monkeypatch) -> None:
        # AC3-EDGE: the cutover window and the defer horizon are independent
        # knobs, so a cutover shorter than the horizon makes both predicates true
        # for one reset. The caller tests cutover first, so without defer winning
        # here a near reset would churn harnesses instead of waiting it out.
        from fno.adapters.providers.runtime_state import evaluate_quota_signal

        self._quota(monkeypatch, defer_dispatch=True, defer_horizon_minutes=60, defer_threshold_pct=90.0)
        # reset in 45 min: inside the 60-min defer horizon AND past a 30-min cutover.
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 95.0, 1000.0 + 2700), probed_at=1000.0), now=1000.0)
        sig = evaluate_quota_signal("p1", priority="p2", cutover_low_after_minutes=30, now=1000.0)
        assert sig.defer and not sig.cutover

    def test_low_outside_horizon_proceeds(self, state_path: Path, monkeypatch) -> None:
        from fno.adapters.providers.runtime_state import evaluate_quota_signal

        self._quota(monkeypatch, defer_dispatch=True, defer_horizon_minutes=60)
        # reset in 2h (> 60 horizon), 95% -> LOW but too far -> proceed.
        write_usage_snapshot(_snap("p1", UsageWindow("5h", 95.0, 1000.0 + 7200), probed_at=1000.0), now=1000.0)
        assert not evaluate_quota_signal("p1", priority="p2", now=1000.0).defer

    def test_unknown_never_strands(self, state_path: Path, monkeypatch) -> None:
        # AC2-FR: a deferred node whose snapshot ages out degrades to UNKNOWN,
        # which never defers -> the next tick dispatches (deferral cannot outlive
        # the evidence). No fresh snapshot -> UNKNOWN. refresh_usage will try to
        # probe; with no provider record it returns None, staying UNKNOWN.
        from fno.adapters.providers.model import ProvidersConfig
        from fno.adapters.providers.runtime_state import evaluate_quota_signal

        self._quota(monkeypatch, defer_dispatch=True)
        monkeypatch.setattr(loader, "load_providers", lambda *a, **k: ProvidersConfig(records=[]))
        assert not evaluate_quota_signal("p1", priority="p2", now=1000.0).defer


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
        import fno.backlog.advance as advance_mod
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
        # did NOT quota-defer: a live claim yields already-dispatching.
        monkeypatch.setattr(
            advance_mod,
            "_node_dispatch_block_reason",
            lambda node_id, cwd: "already-claimed",
        )
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
        rec = ProviderRecord(id="codex-pro", name="Codex", harness="codex", auth="api_key", env={"OPENAI_API_KEY": "x"})
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
        rec = ProviderRecord(id="codex-pro", name="Codex", harness="codex", auth="api_key", env={"OPENAI_API_KEY": "x"})
        monkeypatch.setattr(pcli, "load_providers", lambda *a, **k: ProvidersConfig(records=[rec]))
        monkeypatch.setattr(pcli, "_get_repo_root", lambda: tmp_path)

        now = _t.time()
        write_usage_snapshot(_snap("codex-pro", UsageWindow("5h", 20.0, now + 3600), probed_at=now), now=now)
        assert pcli.required_bot_headroom_check() == []


# ---------------------------------------------------------------------------
# x-4b8d: a fresh probe self-heals a PROVEN false taint (AC4-FR)
# ---------------------------------------------------------------------------


class TestTaintSelfHeal:
    """A tainted slot used to be a terminal state: the probe refused, the taint
    had no clearer, and every quota consumer read UNKNOWN until someone deleted
    a marker file by hand. A fresh probe now asks the reconciliation primitive
    once, and resumes only if identity was PROVEN."""

    @staticmethod
    def _slot(tmp_path, monkeypatch, token: str = "slot-token"):
        monkeypatch.setenv("HOME", str(tmp_path))
        slot = tmp_path / ".claude"
        slot.mkdir(exist_ok=True)
        (slot / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": token}})
        )
        return slot

    @staticmethod
    def _record(record_id: str = "primary") -> ProviderRecord:
        return ProviderRecord(
            id=record_id, name=record_id, harness="claude", auth="managed"
        )

    def test_proven_match_clears_taint_and_reports_real_usage(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC4-FR: reconciliation proves the record, so the probe resumes."""
        import fno.adapters.providers.usage as usage_mod

        self._slot(tmp_path, monkeypatch)
        rec = self._record()
        attributable = {"value": False}
        calls: list[str] = []

        monkeypatch.setattr(
            usage_mod, "_is_active_slot_occupant", lambda r: attributable["value"]
        )

        def _reconcile(cli, *, by_id, root=None, lock_timeout=10):
            calls.append(cli)
            attributable["value"] = True  # the taint cleared
            from fno.adapters.providers.managed import ReconcileResult

            return ReconcileResult("matched", record_id=rec.id, detail="proven")

        # The probe table binds the function at import; patching the module
        # attribute alone would leave the real probe wired up.
        monkeypatch.setitem(usage_mod._PROBES, "claude", lambda r, now: (UsageSnapshot(
            provider_id=r.id,
            windows=(UsageWindow(label="5h", used_pct=22.0, resets_at=now + 60),),
            probed_at=now,
            source="oauth-endpoint",
        ), None))
        self._arm(monkeypatch, usage_mod, rec, _reconcile, tainted=True)

        snap = probe_usage(rec, now=1000.0)

        assert calls == ["claude"]
        assert snap is not None and snap.windows[0].used_pct == 22.0

    def test_unproven_identity_stays_unknown(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC4-FR: reconciliation could not prove the match, so UNKNOWN stands."""
        import fno.adapters.providers.usage as usage_mod

        self._slot(tmp_path, monkeypatch)
        rec = self._record()
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda r: False)
        monkeypatch.setitem(
            usage_mod._PROBES, "claude",
            lambda r, now: pytest.fail("an unproven slot must never be probed"),
        )

        def _reconcile(cli, *, by_id, root=None, lock_timeout=10):
            from fno.adapters.providers.managed import ReconcileResult

            return ReconcileResult("profile-unavailable", detail="endpoint down")

        self._arm(monkeypatch, usage_mod, rec, _reconcile, tainted=True)
        assert probe_usage(rec, now=1000.0) is None

    def test_an_untainted_refusal_never_calls_reconciliation(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A non-occupant is simply not this slot's account: nothing to repair."""
        import fno.adapters.providers.usage as usage_mod

        self._slot(tmp_path, monkeypatch)
        rec = self._record("other")
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda r: False)

        def _reconcile(cli, **kwargs):
            pytest.fail("an untainted slot must not trigger reconciliation")

        self._arm(monkeypatch, usage_mod, rec, _reconcile, tainted=False)
        assert probe_usage(rec, now=1000.0) is None

    def test_config_dir_record_never_reconciles(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Boundaries: its own dir is attributable, so taint cannot touch it."""
        import fno.adapters.providers.usage as usage_mod

        own = tmp_path / "claude-alt"
        own.mkdir()
        rec = ProviderRecord(
            id="alt", name="alt", harness="claude", auth="managed", config_dir=own
        )

        def _reconcile(cli, **kwargs):
            pytest.fail("a config_dir record must never enter slot reconciliation")

        self._arm(monkeypatch, usage_mod, rec, _reconcile, tainted=True)
        monkeypatch.setitem(
            usage_mod._PROBES, "claude", lambda r, now: (None, "probe-failed")
        )
        assert probe_usage(rec, now=1000.0) is None

    def test_a_refusal_is_backed_off_not_retried_every_probe(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Only backoff is cached, never proof: a failure must not hammer the
        endpoint, and it must not become a reason to skip a later repair."""
        import fno.adapters.providers.usage as usage_mod

        self._slot(tmp_path, monkeypatch)
        rec = self._record()
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda r: False)
        calls: list[str] = []

        def _reconcile(cli, *, by_id, root=None, lock_timeout=10):
            calls.append(cli)
            from fno.adapters.providers.managed import ReconcileResult

            return ReconcileResult("zero-match", detail="unregistered principal")

        self._arm(monkeypatch, usage_mod, rec, _reconcile, tainted=True, root=tmp_path)
        probe_usage(rec, now=1000.0)
        probe_usage(rec, now=1000.0)
        assert calls == ["claude"]
        # Past the window the repair is attempted again.
        probe_usage(rec, now=1000.0 + 3600)
        assert calls == ["claude", "claude"]

    @staticmethod
    def _arm(monkeypatch, usage_mod, rec, reconcile_fn, *, tainted: bool, root=None):
        """Point the probe's reconciliation hook at a fake store."""
        from fno.adapters.providers import managed as managed_mod
        from fno.adapters.providers.model import ProvidersConfig

        store = root if root is not None else Path("/nonexistent-store")
        monkeypatch.setattr(managed_mod, "store_root", lambda: store)
        monkeypatch.setattr(
            managed_mod, "slot_tainted", lambda cli, r: tainted
        )
        monkeypatch.setattr(managed_mod, "reconcile_slot", reconcile_fn)
        monkeypatch.setattr(
            usage_mod, "_load_records", lambda: ProvidersConfig(records=[rec]).by_id
        )


class TestUntaintedStampDrift:
    """The taint watches the door footnote controls. `claude /login` uses the
    other one and leaves a stamp that is wrong AND untainted, so attribution
    proceeds confidently and bills the wrong account - observed live."""

    @staticmethod
    def _record(record_id: str = "primary") -> ProviderRecord:
        return ProviderRecord(
            id=record_id, name=record_id, harness="claude", auth="managed"
        )

    @staticmethod
    def _slot(tmp_path, monkeypatch, token: str = "slot-token"):
        monkeypatch.setenv("HOME", str(tmp_path))
        slot = tmp_path / ".claude"
        slot.mkdir(exist_ok=True)
        (slot / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": token}})
        )

    @staticmethod
    def _arm(monkeypatch, rec, *, verdicts, used, reconciled=None, root=None):
        """Point the per-bearer verdict at a lookup table and record the bearer
        the usage request actually spends."""
        from pathlib import Path as _Path

        import fno.adapters.providers.usage as usage_mod
        from fno.adapters.providers import managed as managed_mod
        from fno.adapters.providers.model import ProvidersConfig

        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda r: True)
        monkeypatch.setattr(
            managed_mod, "store_root", lambda: root or _Path("/nonexistent-store")
        )
        monkeypatch.setattr(managed_mod, "slot_tainted", lambda cli, r: False)
        monkeypatch.setattr(
            managed_mod, "bearer_principal_verdict",
            lambda cli, record_id, r, bearer, **kw: verdicts[bearer],
        )
        monkeypatch.setattr(
            managed_mod, "reconcile_slot",
            lambda cli, **kw: (reconciled.append(cli) if reconciled is not None else None)
            or managed_mod.ReconcileResult("zero-match", detail="nothing bound"),
        )
        monkeypatch.setattr(
            usage_mod, "_load_records", lambda: ProvidersConfig(records=[rec]).by_id
        )

        class _Resp:
            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

            def read(self):
                return json.dumps({
                    "five_hour": {
                        "utilization": 22.0, "resets_at": "2026-08-03T10:00:00+00:00"
                    }
                }).encode()

        def _urlopen(req, timeout=None):
            used.append(req.headers["Authorization"].removeprefix("Bearer "))
            return _Resp()

        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", _urlopen)

    def test_the_credential_proven_is_the_credential_measured(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The whole finding: the probe tries several bearers because a stale
        scoped Keychain item 401s while the unscoped one is live. A check that
        proved one credential while the request spent another would report
        account B's usage under account A's name."""
        import fno.adapters.providers.usage as usage_mod

        rec = self._record()
        self._slot(tmp_path, monkeypatch)
        monkeypatch.setattr(
            usage_mod, "_read_claude_keychain_blobs",
            lambda cfg: [json.dumps({"claudeAiOauth": {"accessToken": "unscoped"}})],
        )
        used: list[str] = []
        self._arm(
            monkeypatch, rec,
            verdicts={"slot-token": "mismatch", "unscoped": "match"},
            used=used, root=tmp_path,
        )

        snap = probe_usage(rec, now=1000.0)

        assert snap is not None and snap.windows[0].used_pct == 22.0
        # The mismatching bearer was never spent on a usage request.
        assert used == ["unscoped"]

    def test_every_candidate_unattributable_reports_unknown_and_repairs_once(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        rec = self._record()
        self._slot(tmp_path, monkeypatch)
        used: list[str] = []
        reconciled: list[str] = []
        self._arm(
            monkeypatch, rec, verdicts={"slot-token": "mismatch"},
            used=used, reconciled=reconciled, root=tmp_path,
        )

        assert probe_usage(rec, now=1000.0) is None
        assert used == []  # no other account's usage was even fetched
        assert reconciled == ["claude"]

    def test_an_unprovable_identity_is_refused_not_assumed(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Shared-slot attribution needs FRESH proof. Refusing costs little: the
        usage endpoint that would consume the attribution shares a host with the
        profile endpoint, so an outage hiding identity has already taken the
        measurement with it."""
        rec = self._record()
        self._slot(tmp_path, monkeypatch)
        used: list[str] = []
        self._arm(
            monkeypatch, rec, verdicts={"slot-token": "unprovable"},
            used=used, root=tmp_path,
        )

        assert probe_usage(rec, now=1000.0) is None
        assert used == []

    def test_a_config_dir_record_is_never_checked(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Boundaries: its own dir is attributable without the shared slot."""
        import fno.adapters.providers.usage as usage_mod
        from fno.adapters.providers import managed as managed_mod

        own = tmp_path / "claude-alt"
        own.mkdir()
        (own / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "own-token"}})
        )
        rec = ProviderRecord(
            id="alt", name="alt", harness="claude", auth="managed", config_dir=own
        )
        monkeypatch.setattr(
            managed_mod, "bearer_principal_verdict",
            lambda *a, **k: pytest.fail("a config_dir record consulted the slot"),
        )
        used: list[str] = []

        class _Resp:
            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

            def read(self):
                return json.dumps({
                    "five_hour": {
                        "utilization": 4.0, "resets_at": "2026-08-03T10:00:00+00:00"
                    }
                }).encode()

        def _urlopen(req, timeout=None):
            used.append(req.headers["Authorization"])
            return _Resp()

        monkeypatch.setattr(usage_mod.urllib.request, "urlopen", _urlopen)
        assert probe_usage(rec, now=1000.0) is not None
        assert len(used) == 1

    def test_a_codex_record_never_consults_a_principal(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """codex can never prove a slot principal, so refusing there would
        silence its measurement permanently for no gain. Its probe simply does
        not go through the bearer check."""
        from fno.adapters.providers import managed as managed_mod

        monkeypatch.setattr(
            managed_mod, "bearer_principal_verdict",
            lambda *a, **k: pytest.fail("a codex probe consulted a principal"),
        )
        sessions = tmp_path / "sessions"
        sessions.mkdir()
        (sessions / "s.jsonl").write_text(json.dumps({
            "type": "event_msg",
            "payload": {"rate_limits": {
                "primary": {"used_percent": 4.0, "resets_at": 1783807404},
            }},
        }))
        rec = ProviderRecord(
            id="cx", name="cx", harness="codex", auth="oauth_dir",
            credentials_source=tmp_path,
        )
        assert probe_usage(rec, now=1000.0) is not None


class TestPrincipalEvidenceTTL:
    def test_proven_evidence_is_reused_then_expires(self, tmp_path: Path) -> None:
        """Cached briefly so an attribution check costs no call on the common
        path."""
        from fno.adapters.providers import managed

        managed.note_slot_principal("claude", tmp_path, "acct-a", "tok-1", now=1000.0)
        assert managed.cached_slot_principal(
            "claude", tmp_path, "tok-1", now=1000.0 + 60
        ) == "acct-a"
        assert managed.cached_slot_principal(
            "claude", tmp_path, "tok-1", now=1000.0 + 100_000
        ) is None

    def test_a_changed_credential_invalidates_the_cache_within_the_ttl(
        self, tmp_path: Path
    ) -> None:
        """Time alone is the wrong key: an out-of-band /login inside the TTL
        would otherwise reuse evidence about the credential it replaced, and the
        check built to catch that login would be the thing hiding it."""
        from fno.adapters.providers import managed

        managed.note_slot_principal("claude", tmp_path, "acct-a", "tok-1", now=1000.0)
        assert managed.cached_slot_principal(
            "claude", tmp_path, "tok-2", now=1000.0 + 60
        ) is None

    def test_an_unbound_record_is_unprovable(self, tmp_path: Path) -> None:
        from fno.adapters.providers import managed

        assert managed.bearer_principal_verdict(
            "claude", "never-bound", tmp_path, "tok-1"
        ) == "unprovable"


class TestAmbiguousSlotIsNotAttributable:
    def test_two_credentials_in_the_slot_report_unknown(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """claude reads the scoped Keychain item first while this probe reads
        the unscoped one, so a bearer that proves out here can still be a
        different account from the one actually being billed."""
        import fno.adapters.providers.usage as usage_mod
        from fno.adapters.providers import managed as managed_mod

        monkeypatch.setenv("HOME", str(tmp_path))
        slot = tmp_path / ".claude"
        slot.mkdir()
        (slot / ".credentials.json").write_text(
            json.dumps({"claudeAiOauth": {"accessToken": "unscoped"}})
        )
        rec = ProviderRecord(
            id="primary", name="primary", harness="claude", auth="managed"
        )
        monkeypatch.setattr(usage_mod, "_is_active_slot_occupant", lambda r: True)
        monkeypatch.setattr(managed_mod, "slot_tainted", lambda cli, r: False)
        monkeypatch.setattr(
            managed_mod, "canonical_slot_blobs", lambda cli: ["scoped-a", "unscoped-b"]
        )
        monkeypatch.setattr(
            managed_mod, "bearer_principal_verdict",
            lambda *a, **k: pytest.fail("asked about one bearer in an ambiguous slot"),
        )
        monkeypatch.setattr(
            managed_mod, "reconcile_slot",
            lambda cli, **kw: managed_mod.ReconcileResult("ambiguous-slot", detail="two"),
        )
        # Exercise the real probe: the check lives per-bearer inside it, so
        # stubbing the probe out would test nothing.
        monkeypatch.setattr(
            usage_mod.urllib.request, "urlopen",
            lambda *a, **k: pytest.fail("queried usage for an ambiguous slot"),
        )

        assert probe_usage(rec, now=1000.0) is None
