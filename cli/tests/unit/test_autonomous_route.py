"""select_autonomous_route: the one quota-aware route decision (x-2716).

The probe itself is covered by cli/src/fno/adapters/providers/test_usage.py;
here the signal is injected so the tests pin POLICY - precedence, the inverted
LOW predicate, and the refusal to return a half-resolved destination.
"""

from __future__ import annotations

import json
from types import SimpleNamespace

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
    def test_ac4_hp_real_probe_cuts_over_to_the_healthy_record(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path
    ) -> None:
        import time
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.model import QuotaConfig
        from fno.adapters.providers.rotation import Combo, next_healthy_provider
        from fno.adapters.providers.runtime_state import (
            PROVIDER_HEALTH_TTL_SECONDS,
            UsageSnapshot,
            UsageWindow,
            update_provider_health,
            write_usage_snapshot,
        )
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime.json"))
        quota = QuotaConfig(observe=True, defer_dispatch=True)
        monkeypatch.setattr(
            "fno.adapters.providers.loader.load_quota_config",
            lambda repo_root=None: quota,
        )
        now = time.time()
        write_usage_snapshot(
            UsageSnapshot(
                provider_id="source",
                windows=(UsageWindow("5h", 100.0, now + 3600),),
                probed_at=now,
                source="quota-endpoint",
            ),
            now=now,
        )
        update_provider_health(
            "healthy", ErrorRule(status=429, backoff=True),
            now=now - PROVIDER_HEALTH_TTL_SECONDS - 1,
            resets_at=now + 3600,
        )
        def destination(_cwd, _exhausted):
            chosen = next_healthy_provider(
                Combo(name="fallback", providers=("healthy",)), quota=quota
            )
            return (chosen, "codex", {}) if chosen else None
        monkeypatch.setattr(ar, "_select_destination", destination)
        route = ar.select_autonomous_route(
            provider_id="source", node_cwd=str(tmp_path), now=now
        )
        assert route.action == "cutover"
        assert route.record_id == "healthy"

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
    @pytest.mark.parametrize(
        "kwargs",
        [
            {"provider": "ccm"},
            {"model": "claude-opus-5"},
            {"account": "ccr"},
            {"node": {"provider": "ccm"}},
            {"node": {"model": "claude-opus-5"}},
            {"node": {"harness": "claude"}},
        ],
    )
    def test_every_explicit_intent_counts_as_a_pin(self, monkeypatch, kwargs) -> None:
        # AC4-LOCK: a model pin pins as hard as a provider one - a cutover swaps
        # the harness, so a claude-only model must never ride one onto codex.
        import fno.config as cfg

        # The pin must be decided from the explicit intent alone; reaching config
        # at all means the precedence order is wrong.
        monkeypatch.setattr(
            cfg,
            "load_settings",
            lambda *a, **k: pytest.fail("config read before the explicit pin won"),
        )
        assert ar.launch_is_pinned(**kwargs) is True

    def test_configured_dispatch_harness_pins(self, monkeypatch) -> None:
        # Precedence: configured dispatch harness outranks quota policy, so it
        # must block an automatic reroute the same way an invocation pin does.
        import fno.config as cfg

        monkeypatch.setattr(
            cfg,
            "load_settings",
            lambda *a, **k: SimpleNamespace(dispatch=SimpleNamespace(harness="codex")),
        )
        assert ar.launch_is_pinned({}) is True

    def test_stage_table_harness_pins(self, monkeypatch) -> None:
        # The stage table is the home for the harness axis, so a launch routed
        # by agents.profiles.<verb>.provider pins exactly like the legacy key
        # does - quota policy must not silently replace either.
        import fno.config as cfg

        agents = SimpleNamespace(
            profiles={"target": SimpleNamespace(provider="codex")}
        )
        monkeypatch.setattr(
            cfg,
            "load_settings",
            lambda *a, **k: SimpleNamespace(agents=agents, dispatch=None),
        )
        assert ar.launch_is_pinned({}) is True

    def test_unreadable_config_pins_nothing(self, monkeypatch) -> None:
        import fno.config as cfg

        monkeypatch.setattr(
            cfg, "load_settings", lambda *a, **k: (_ for _ in ()).throw(OSError("nope"))
        )
        assert ar.launch_is_pinned({}) is False

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


# ---------------------------------------------------------------------------
# _select_destination: the combo walk (moved from test_advance.py with the
# function itself - a spawn stages a RECORD id, a harness, and an account env
# together or not at all).
# ---------------------------------------------------------------------------


def test_select_destination_not_configured_defers(monkeypatch):
    """_select_destination: on_exhaustion != failover -> None (no combo read)."""
    from fno.config import SettingsModel

    monkeypatch.setattr("fno.config.load_settings", lambda *a, **k: SettingsModel())
    monkeypatch.setattr(
        "fno.agents.dispatch_target.resolve_dispatch_target",
        lambda *a, **k: pytest.fail("defer must not read the active combo"),
    )
    assert ar._select_destination(None, "ccm") is None


def test_select_destination_configured_picks_provider_and_cli(monkeypatch):
    """_select_destination: failover + a combo with a healthy provider ->
    (record_id, harness, account_env); the record's harness is used directly and its
    dispatch_env becomes the spawn account env."""
    from fno.adapters.providers.rotation import Combo
    from fno.config import SettingsModel
    from fno.agents.dispatch_target import DispatchTarget

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda *a, **k: SettingsModel(dispatch={"on_exhaustion": "failover"}),
    )
    monkeypatch.setattr(
        "fno.agents.dispatch_target.resolve_dispatch_target",
        lambda *a, **k: DispatchTarget(combo_name="combo1"),
    )
    combo = Combo(name="combo1", providers=("ccm", "ccr"))
    monkeypatch.setattr("fno.adapters.providers.loader.load_combos", lambda *a, **k: {"combo1": combo})
    monkeypatch.setattr(
        "fno.adapters.providers.rotation.next_healthy_provider",
        lambda combo, exclude=(), **k: "ccr",
    )
    monkeypatch.setattr(
        "fno.adapters.providers.loader.load_providers",
        lambda *a, **k: SimpleNamespace(by_id={"ccr": SimpleNamespace(harness="codex")}),
    )
    monkeypatch.setattr(
        "fno.adapters.providers.dispatch.dispatch_env",
        lambda pid, repo_root=None: {"CODEX_HOME": "/acct/ccr"},
    )
    assert ar._select_destination(None, "ccm") == (
        "ccr", "codex", {"CODEX_HOME": "/acct/ccr"},
    )


def test_select_destination_unstaged_account_defers(monkeypatch):
    """_select_destination: dispatch_env raising (account not staged) ->
    None (defer; never spawn onto a broken account)."""
    from fno.adapters.providers.rotation import Combo
    from fno.config import SettingsModel
    from fno.agents.dispatch_target import DispatchTarget

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda *a, **k: SettingsModel(dispatch={"on_exhaustion": "failover"}),
    )
    monkeypatch.setattr(
        "fno.agents.dispatch_target.resolve_dispatch_target",
        lambda *a, **k: DispatchTarget(combo_name="combo1"),
    )
    combo = Combo(name="combo1", providers=("ccm", "ccr"))
    monkeypatch.setattr("fno.adapters.providers.loader.load_combos", lambda *a, **k: {"combo1": combo})
    monkeypatch.setattr(
        "fno.adapters.providers.rotation.next_healthy_provider",
        lambda combo, exclude=(), **k: "ccr",
    )
    monkeypatch.setattr(
        "fno.adapters.providers.loader.load_providers",
        lambda *a, **k: SimpleNamespace(by_id={"ccr": SimpleNamespace(harness="claude")}),
    )

    def boom(pid, repo_root=None):
        raise RuntimeError("account not staged")

    monkeypatch.setattr("fno.adapters.providers.dispatch.dispatch_env", boom)
    assert ar._select_destination(None, "ccm") is None


def test_select_destination_no_active_combo_defers(monkeypatch):
    """_select_destination: failover configured but the active target is a
    bare provider (no combo) -> None (nothing to walk)."""
    from fno.config import SettingsModel
    from fno.agents.dispatch_target import DispatchTarget

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda *a, **k: SettingsModel(dispatch={"on_exhaustion": "failover"}),
    )
    monkeypatch.setattr(
        "fno.agents.dispatch_target.resolve_dispatch_target",
        lambda *a, **k: DispatchTarget(provider_id="ccm", source="active_provider"),
    )
    assert ar._select_destination(None, "ccm") is None


class TestAlternateAccountScope:
    """The defer escape must not be answered from the wrong repository."""

    def test_cross_project_node_leaves_the_defer_standing(self, monkeypatch, tmp_path) -> None:
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        _dest(monkeypatch, None)
        monkeypatch.setattr(
            "fno.adapters.providers.cli.pick_account",
            lambda **k: pytest.fail("read the dispatcher's accounts for a foreign node"),
        )
        assert _route(node_cwd=str(tmp_path)).action == "defer"

    def test_same_project_node_consults_the_picker(self, monkeypatch, tmp_path) -> None:
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        _dest(monkeypatch, None)
        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(
            "fno.adapters.providers.cli.pick_account",
            lambda **k: SimpleNamespace(account="ccr"),
        )
        r = _route(node_cwd=str(tmp_path))
        assert r.action == "stay"
        assert r.reason == "alternate-account-available"

    def test_a_pinned_launch_never_takes_the_escape(self, monkeypatch) -> None:
        # Picking is a reroute, and a pin forbids reroutes - not defers.
        _signal(monkeypatch, state=HeadroomState.EXHAUSTED, defer=True, cutover=True)
        monkeypatch.setattr(
            "fno.adapters.providers.cli.pick_account",
            lambda **k: pytest.fail("rerouted a pinned launch to another account"),
        )
        assert _route(pinned=True).action == "defer"


# Quota exhaustion must not change who may merge. x-e53e deleted the pre-render
# (`_cutover_command`) this class pinned; the carrier now is the subprocess
# env - the cutover shellout sets TARGET_NO_MERGE=1 unconditionally, pinned in
# test_dispatch_one.py::test_cutover_pins_harness_and_record_and_no_merge.


def test_a_configured_harness_pins_the_launch(monkeypatch) -> None:
    """config.dispatch.harness is a choice the launch honors (x-e53e deleted
    the one dispatcher that hardcoded its harness and needed an opt-out), so
    the configured rung pins: a cutover must never override it."""
    import fno.config as cfg

    monkeypatch.setattr(
        cfg,
        "load_settings",
        lambda *a, **k: SimpleNamespace(dispatch=SimpleNamespace(harness="codex")),
    )
    assert ar.launch_is_pinned({}) is True
    # An explicit account pin needs no config at all.
    assert ar.launch_is_pinned({}, account="ccr") is True


class TestQuotaRotationDeclinedEvent:
    """unknown-proceed is a launch on blind headroom, not a healthy no-op -
    an absent event and a healthy system must not read the same in the
    journal (the assert-a-positive-marker pitfall)."""

    def _events(self, tmp_path):
        from tests._event_rows import event_rows

        return event_rows(tmp_path / ".fno" / "events.jsonl")

    def test_unknown_proceed_emits_exactly_one_declined_event(
        self, monkeypatch, tmp_path,
    ) -> None:
        monkeypatch.chdir(tmp_path)
        # The journal too. The hermetic sandbox pins FNO_EVENTS_PATH for the
        # whole pytest process, and project_events_json checks it ahead of the
        # cwd-derived root, so a test reading tmp_path's journal must name it.
        monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime-state.json"))
        _signal(
            monkeypatch, state=HeadroomState.UNKNOWN, defer=False, cutover=False,
            resets_at=None, reason="defer-dispatch-off",
        )

        r = ar.select_autonomous_route(provider_id="ccm", node_id="fake-node-1")

        assert r.action == "unknown-proceed"
        events = self._events(tmp_path)
        assert len(events) == 1
        assert events[0]["type"] == "quota_rotation_declined"
        assert events[0]["data"] == {
            "provider": "ccm", "reason": "defer-dispatch-off", "node_id": "fake-node-1",
        }

    def test_no_usage_snapshot_omits_age_and_the_event_still_lands(
        self, monkeypatch, tmp_path,
    ) -> None:
        # AC2: read_usage() with no snapshot ever written returns None, so
        # snapshot_age_s is simply absent - the event must still validate and
        # append (both `provider` and `reason` are its only required fields).
        monkeypatch.chdir(tmp_path)
        # The journal too. The hermetic sandbox pins FNO_EVENTS_PATH for the
        # whole pytest process, and project_events_json checks it ahead of the
        # cwd-derived root, so a test reading tmp_path's journal must name it.
        monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime-state.json"))
        _signal(
            monkeypatch, state=HeadroomState.UNKNOWN, defer=False, cutover=False,
            resets_at=None, reason="no-provider",
        )

        ar.select_autonomous_route(provider_id="ccm")  # no node_id this time

        events = self._events(tmp_path)
        assert len(events) == 1
        assert "snapshot_age_s" not in events[0]["data"]
        assert "node_id" not in events[0]["data"]

    def test_append_event_failure_is_swallowed_and_route_is_unchanged(
        self, monkeypatch, tmp_path,
    ) -> None:
        import fno.events as events_mod

        monkeypatch.chdir(tmp_path)
        # The journal too. The hermetic sandbox pins FNO_EVENTS_PATH for the
        # whole pytest process, and project_events_json checks it ahead of the
        # cwd-derived root, so a test reading tmp_path's journal must name it.
        monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
        monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime-state.json"))
        _signal(
            monkeypatch, state=HeadroomState.UNKNOWN, defer=False, cutover=False,
            resets_at=None, reason="not-probed",
        )

        def _boom(*_a, **_k):
            raise OSError("simulated disk failure")

        monkeypatch.setattr(events_mod, "append_event", _boom)

        r = ar.select_autonomous_route(provider_id="ccm")

        assert r.action == "unknown-proceed"
        assert r.reason == "not-probed"
        assert not (tmp_path / ".fno" / "events.jsonl").exists()
