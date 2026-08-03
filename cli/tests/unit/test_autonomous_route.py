"""select_autonomous_route: the one quota-aware route decision (x-2716).

The probe itself is covered by cli/src/fno/adapters/providers/test_usage.py;
here the signal is injected so the tests pin POLICY - precedence, the inverted
LOW predicate, and the refusal to return a half-resolved destination.
"""

from __future__ import annotations

import pytest

from fno.adapters.providers import runtime_state as rs
from fno.adapters.providers.runtime_state import HeadroomState, QuotaSignal
from fno.agents import autonomous_route as ar

DEST = ("ccr", "codex", {"CODEX_HOME": "/tmp/ccr"})


def _signal(monkeypatch, *, state, defer, cutover, resets_at=9e18, reason="probed"):
    monkeypatch.setattr(
        rs,
        "evaluate_quota_signal",
        lambda pid, **kw: QuotaSignal("ccm", state, resets_at, defer, cutover, reason),
    )


def _dest(monkeypatch, value):
    monkeypatch.setattr(ar, "_select_destination", lambda cwd, exhausted: value)


def _route(**kw):
    return ar.select_autonomous_route(provider_id="ccm", **kw)


class TestRouteActions:
    def test_exhausted_cuts_over_to_the_other_harness(self, monkeypatch) -> None:
        # AC1-HP: exhausted claude + healthy codex candidate -> cutover, with
        # the complete destination tuple a spawn needs.
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        _dest(monkeypatch, DEST)
        r = _route()
        assert r.action == "cutover"
        assert (r.record_id, r.harness, r.account_env) == DEST
        assert r.source_record == "ccm"
        assert r.window == "exhausted"

    def test_distant_low_cuts_over(self, monkeypatch) -> None:
        # AC2-HP: the inverted predicate - a LOW window resetting far away is a
        # reason to leave NOW, and it does not defer.
        _signal(monkeypatch, state=HeadroomState.LOW, defer=False, cutover=True)
        _dest(monkeypatch, DEST)
        assert _route().action == "cutover"

    def test_nearby_low_defers_and_never_cuts_over(self, monkeypatch) -> None:
        # AC3-EDGE: a near reset keeps the existing keep-or-defer policy, so the
        # harness does not churn.
        _signal(monkeypatch, state=HeadroomState.LOW, defer=True, cutover=False)
        _dest(monkeypatch, DEST)
        r = _route()
        assert r.action == "defer"
        assert r.record_id is None

    def test_exhausted_without_candidate_falls_to_defer(self, monkeypatch) -> None:
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        _dest(monkeypatch, None)
        r = _route()
        assert r.action == "defer"
        assert r.retry_at == 9e18

    def test_distant_low_without_candidate_stays(self, monkeypatch) -> None:
        # cutover-only signal + no destination: nothing to defer for either, so
        # the launch proceeds here rather than stalling on a non-binding window.
        _signal(monkeypatch, state=HeadroomState.LOW, defer=False, cutover=True)
        _dest(monkeypatch, None)
        assert _route().action == "stay"

    @pytest.mark.parametrize("reason", ["defer-dispatch-off", "p0-exempt", "no-provider"])
    def test_unprobed_proceeds_without_reading_the_combo(self, monkeypatch, reason) -> None:
        _signal(
            monkeypatch,
            state=HeadroomState.UNKNOWN,
            defer=False,
            cutover=False,
            resets_at=None,
            reason=reason,
        )
        monkeypatch.setattr(
            ar, "_select_destination", lambda *a: pytest.fail("combo read on an unprobed signal")
        )
        r = _route()
        assert r.action == "unknown-proceed"
        assert r.reason == reason

    def test_ok_stays(self, monkeypatch) -> None:
        _signal(monkeypatch, state=HeadroomState.OK, defer=False, cutover=False, resets_at=None)
        assert _route().action == "stay"


class TestExplicitIntentWins:
    def test_pinned_exhausted_defers_instead_of_rerouting(self, monkeypatch) -> None:
        # AC4-LOCK: quota policy never replaces a harness/account a human chose.
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        monkeypatch.setattr(
            ar, "_select_destination", lambda *a: pytest.fail("pinned launch was rerouted")
        )
        r = _route(pinned=True)
        assert r.action == "defer"
        assert r.reason == "pinned"

    def test_pinned_distant_low_stays(self, monkeypatch) -> None:
        _signal(monkeypatch, state=HeadroomState.LOW, defer=False, cutover=True)
        monkeypatch.setattr(
            ar, "_select_destination", lambda *a: pytest.fail("pinned launch was rerouted")
        )
        assert _route(pinned=True).action == "stay"


class TestUnresolvableDestination:
    """AC5-FR: a destination missing either half is never launched."""

    @pytest.mark.parametrize(
        "broken",
        [
            "no-combo",  # resolve_dispatch_target has no active combo
            "no-harness",  # the record carries no cli
            "account-error",  # the account overlay cannot be staged
        ],
    )
    def test_every_unresolvable_path_is_the_defer_floor(self, monkeypatch, broken) -> None:
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        _dest(monkeypatch, None)  # _select_destination degrades all three to None
        assert _route().action == "defer", broken

    def test_selector_never_returns_a_partial_tuple(self, monkeypatch) -> None:
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        _dest(monkeypatch, DEST)
        r = _route()
        assert all(v is not None for v in (r.record_id, r.harness, r.account_env))


class TestCutoverConfig:
    def test_unreadable_config_disarms_proactive_cutover(self, monkeypatch) -> None:
        def boom(*a, **k):
            raise RuntimeError("unreadable")

        monkeypatch.setattr("fno.config.load_settings", boom)
        assert ar._cutover_low_after_minutes(None) == 0

    def test_negative_and_non_int_values_degrade_to_off(self) -> None:
        from fno.config import DispatchBlock

        for bad in (-30, True, "60", 1.5, None):
            assert DispatchBlock(cutover_low_after_minutes=bad).cutover_low_after_minutes == 0
        assert DispatchBlock(cutover_low_after_minutes=60).cutover_low_after_minutes == 60
