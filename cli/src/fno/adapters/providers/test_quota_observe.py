"""Looking and acting are two decisions (x-763a, change 4).

Run: cd cli && uv run pytest src/fno/adapters/providers/test_quota_observe.py -v

One flag used to decide both "may fno LOOK at quota" and "may fno DEFER a
dispatch because of it", so an operator who wanted a working meter had to
accept automatic deferral and the safe choice was blindness.

AC7-EDGE asserts NO NETWORK CALL rather than an unknown verdict, because an
unknown verdict is also what a FAILED probe returns: the two are only
distinguishable by whether the probe was ever attempted.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.adapters.providers import runtime_state
from fno.adapters.providers.model import QuotaConfig
from fno.adapters.providers.runtime_state import (
    HeadroomState,
    UsageSnapshot,
    UsageWindow,
    evaluate_quota_signal,
)


@pytest.fixture
def probe_recorder(monkeypatch: pytest.MonkeyPatch):
    """Count refresh_usage calls: the positive marker that a probe RAN."""
    calls: list[str] = []

    def fake_refresh(provider_id, **kw):
        calls.append(provider_id)
        return UsageSnapshot(
            provider_id=provider_id,
            windows=(UsageWindow("5h", 12.0, kw.get("now", 0.0) + 3600.0),),
            probed_at=kw.get("now", 0.0),
            source="quota-endpoint",
            confidence="exact",
        )
    monkeypatch.setattr(runtime_state, "refresh_usage", fake_refresh)
    monkeypatch.setattr(
        runtime_state,
        "refresh_usage_detailed",
        lambda provider_id, **kw: runtime_state.UsageRefresh(
            fake_refresh(provider_id, **kw)
        ),
    )
    return calls


def _quota(monkeypatch: pytest.MonkeyPatch, **kw) -> None:
    cfg = QuotaConfig(**kw)
    monkeypatch.setattr(
        "fno.adapters.providers.loader.load_quota_config", lambda repo_root=None: cfg
    )


class TestObserveSplitsLookingFromActing:
    def test_observe_on_defer_off_probes_and_does_not_defer(
        self, monkeypatch: pytest.MonkeyPatch, probe_recorder, tmp_path: Path
    ) -> None:
        """AC6-HP. A probe RUNS and the dispatch is NOT deferred. Asserting a
        snapshot exists is insufficient; assert the dispatch proceeded."""
        _quota(monkeypatch, observe=True, defer_dispatch=False)
        sig = evaluate_quota_signal("zai", now=1000.0, repo_root=tmp_path)
        assert probe_recorder == ["zai"], "observe=true must run the probe"
        assert sig.defer is False
        assert sig.cutover is False
        assert sig.reason == "observed"
        assert sig.state is not HeadroomState.UNKNOWN

    def test_both_unset_makes_no_network_call_at_all(
        self, monkeypatch: pytest.MonkeyPatch, probe_recorder, tmp_path: Path
    ) -> None:
        """AC7-EDGE. Today's config, byte for byte: a fresh install must not
        start making network calls."""
        _quota(monkeypatch, observe=False, defer_dispatch=False)
        sig = evaluate_quota_signal("zai", now=1000.0, repo_root=tmp_path)
        assert probe_recorder == [], "an unarmed sensor must not probe"
        assert sig.state is HeadroomState.UNKNOWN
        assert sig.defer is False and sig.cutover is False
        assert sig.reason == "quota-observation-off"

    def test_defer_dispatch_implies_observation(
        self, monkeypatch: pytest.MonkeyPatch, probe_recorder, tmp_path: Path
    ) -> None:
        """AC8-HP. Deferring requires looking, so the stronger flag subsumes
        the weaker and a config that works today keeps working."""
        _quota(monkeypatch, observe=False, defer_dispatch=True)
        sig = evaluate_quota_signal("zai", now=1000.0, repo_root=tmp_path)
        assert probe_recorder == ["zai"]
        assert sig.reason == "probed"

    def test_an_exhausted_provider_still_does_not_defer_under_observe_only(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        """The whole point of the split: seeing a wall changes what fno KNOWS,
        not what it does, until deferral is armed separately."""
        _quota(monkeypatch, observe=True, defer_dispatch=False)
        def fake_refresh(provider_id, **kw):
            return UsageSnapshot(
                provider_id=provider_id,
                windows=(UsageWindow("5h", 100.0, kw.get("now", 0.0) + 3600.0),),
                probed_at=kw.get("now", 0.0),
                source="quota-endpoint",
            )
        monkeypatch.setattr(
            runtime_state,
            "refresh_usage_detailed",
            lambda provider_id, **kw: runtime_state.UsageRefresh(
                fake_refresh(provider_id, **kw)
            ),
        )
        sig = evaluate_quota_signal("zai", now=1000.0, repo_root=tmp_path)
        assert sig.state is HeadroomState.EXHAUSTED
        assert sig.defer is False and sig.cutover is False

    def test_p0_is_still_exempt_when_observation_is_armed(
        self, monkeypatch: pytest.MonkeyPatch, probe_recorder, tmp_path: Path
    ) -> None:
        _quota(monkeypatch, observe=True, defer_dispatch=True)
        sig = evaluate_quota_signal("zai", priority="p0", now=1000.0, repo_root=tmp_path)
        assert probe_recorder == []
        assert sig.reason == "p0-exempt"

    def test_ac2_err_credential_probe_failure_stays_unknown_and_eligible(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        import time
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.rotation import Combo, next_healthy_provider
        from fno.adapters.providers.runtime_state import (
            PROVIDER_HEALTH_TTL_SECONDS,
            UsageRefresh,
            update_provider_health,
        )
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime.json"))
        quota = QuotaConfig(observe=True, defer_dispatch=True)
        monkeypatch.setattr(
            "fno.adapters.providers.loader.load_quota_config",
            lambda repo_root=None: quota,
        )
        now = time.time()
        update_provider_health(
            "p1", ErrorRule(status=429, backoff=True),
            now=now - PROVIDER_HEALTH_TTL_SECONDS - 1,
            resets_at=now + 3600,
        )
        monkeypatch.setattr(
            runtime_state,
            "refresh_usage_detailed",
            lambda *args, **kwargs: UsageRefresh(None, "credential-rejected"),
        )
        sig = evaluate_quota_signal("p1", priority="p2", now=now, repo_root=tmp_path)
        assert sig.state is HeadroomState.UNKNOWN
        assert sig.defer is False and sig.cutover is False
        assert sig.reason == "credential-rejected"
        assert next_healthy_provider(
            Combo(name="fallback", providers=("p1",)), quota=quota
        ) == "p1"

class TestQuotaConfigDefaults:
    def test_observe_defaults_off(self) -> None:
        assert QuotaConfig().observe is False

    def test_observe_survives_a_neighbouring_invalid_leaf(self) -> None:
        """The loader salvages leaf by leaf, so a bad sibling key must not
        discard an armed observe."""
        from fno.adapters.providers.loader import _quota_leaf_valid

        assert _quota_leaf_valid("observe", True) is True
        assert _quota_leaf_valid("defer_threshold_pct", 900.0) is False
