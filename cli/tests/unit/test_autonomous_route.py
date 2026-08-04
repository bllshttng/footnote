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
        "fno.sigma_dispatch.resolve_dispatch_target",
        lambda *a, **k: pytest.fail("defer must not read the active combo"),
    )
    assert ar._select_destination(None, "ccm") is None


def test_select_destination_configured_picks_provider_and_cli(monkeypatch):
    """_select_destination: failover + a combo with a healthy provider ->
    (record_id, harness, account_env); the record's harness is used directly and its
    dispatch_env becomes the spawn account env."""
    from fno.adapters.providers.rotation import Combo
    from fno.config import SettingsModel
    from fno.sigma_dispatch import DispatchTarget

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda *a, **k: SettingsModel(dispatch={"on_exhaustion": "failover"}),
    )
    monkeypatch.setattr(
        "fno.sigma_dispatch.resolve_dispatch_target",
        lambda *a, **k: DispatchTarget(combo_name="combo1"),
    )
    combo = Combo(name="combo1", providers=("ccm", "ccr"))
    monkeypatch.setattr("fno.adapters.providers.loader.load_combos", lambda *a, **k: {"combo1": combo})
    monkeypatch.setattr(
        "fno.adapters.providers.rotation.next_healthy_provider",
        lambda combo, exclude=(): "ccr",
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
    from fno.sigma_dispatch import DispatchTarget

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda *a, **k: SettingsModel(dispatch={"on_exhaustion": "failover"}),
    )
    monkeypatch.setattr(
        "fno.sigma_dispatch.resolve_dispatch_target",
        lambda *a, **k: DispatchTarget(combo_name="combo1"),
    )
    combo = Combo(name="combo1", providers=("ccm", "ccr"))
    monkeypatch.setattr("fno.adapters.providers.loader.load_combos", lambda *a, **k: {"combo1": combo})
    monkeypatch.setattr(
        "fno.adapters.providers.rotation.next_healthy_provider",
        lambda combo, exclude=(): "ccr",
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
    from fno.sigma_dispatch import DispatchTarget

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda *a, **k: SettingsModel(dispatch={"on_exhaustion": "failover"}),
    )
    monkeypatch.setattr(
        "fno.sigma_dispatch.resolve_dispatch_target",
        lambda *a, **k: DispatchTarget(provider_id="ccm", source="active_provider"),
    )
    assert ar._select_destination(None, "ccm") is None


class TestAutonomousResolveRung:
    """`fno dispatch resolve --autonomous`: the seam the shell dispatcher uses.

    dispatch-node.sh reached only the pure resolver, so /target bg and blueprint
    auto-launch never saw a quota verdict. These pin the folded tuple.
    """

    def _resolve(self, monkeypatch, route, *, harness=None, node="ab-1111aaaa"):
        import fno.dispatch as dm
        from typer.testing import CliRunner

        monkeypatch.setattr(dm, "_lookup_node", lambda ref: {"id": node, "priority": "p2"})
        monkeypatch.setattr(dm, "_autonomous_route_for", lambda *a, **k: route)
        args = ["resolve", "--autonomous", "--node", node, "-J"]
        if harness:
            args += ["--harness", harness]
        return CliRunner().invoke(dm.dispatch_app, args)

    def test_cutover_returns_the_destination_harness_and_command(self, monkeypatch) -> None:
        route = ar.AutonomousRoute(
            "cutover",
            "exhausted-cutover",
            source_record="ccm",
            record_id="ccr",
            harness="codex",
            account_env={"CODEX_HOME": "/acct/ccr"},
            window="exhausted",
        )
        out = json.loads(self._resolve(monkeypatch, route).stdout)
        assert out["route_action"] == "cutover"
        assert out["harness"] == "codex"
        # Codex takes its own command surface, never a raw claude slash verb.
        assert out["command"].startswith("$fno:target")
        # The record id crosses the process boundary; the credentials never do.
        assert out["route_account"] == "ccr"
        assert "CODEX_HOME" not in json.dumps(out)

    def test_defer_is_reported_so_the_shell_can_park(self, monkeypatch) -> None:
        route = ar.AutonomousRoute(
            "defer", "exhausted", source_record="ccm", retry_at=9e18, window="exhausted"
        )
        out = json.loads(self._resolve(monkeypatch, route).stdout)
        assert out["route_action"] == "defer"
        assert out["route_source"] == "ccm"
        assert out["route_retry_at"] == 9e18

    def test_unrenderable_destination_falls_back_to_defer_not_the_walled_harness(
        self, monkeypatch
    ) -> None:
        """A cutover that cannot render is not a cutover, but the quota verdict
        behind it stands: falling back to the original harness would launch on
        the very account the selector ruled out."""
        route = ar.AutonomousRoute(
            "cutover",
            "exhausted-cutover",
            source_record="ccm",
            record_id="ghost",
            harness="no-such-harness",
            account_env={"X": "1"},
            window="exhausted",
            defer_fallback=True,
        )
        out = json.loads(self._resolve(monkeypatch, route).stdout)
        assert out["route_action"] == "defer"
        assert out["route_reason"].endswith("destination-unrenderable")
        assert out["route_account"] == ""

    def test_unrenderable_destination_on_a_nonbinding_low_stays(self, monkeypatch) -> None:
        route = ar.AutonomousRoute(
            "cutover",
            "low-cutover",
            source_record="ccm",
            record_id="ghost",
            harness="no-such-harness",
            account_env={"X": "1"},
            window="low",
            defer_fallback=False,
        )
        out = json.loads(self._resolve(monkeypatch, route).stdout)
        assert out["route_action"] == "stay"

    def test_bare_resolve_stays_pure(self, monkeypatch) -> None:
        import fno.dispatch as dm
        from typer.testing import CliRunner

        monkeypatch.setattr(dm, "_lookup_node", lambda ref: {"id": "ab-1111aaaa"})
        monkeypatch.setattr(
            dm,
            "_autonomous_route_for",
            lambda *a, **k: pytest.fail("bare resolve probed quota"),
        )
        out = json.loads(
            CliRunner().invoke(dm.dispatch_app, ["resolve", "--node", "ab-1111aaaa", "-J"]).stdout
        )
        assert "route_action" not in out


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
