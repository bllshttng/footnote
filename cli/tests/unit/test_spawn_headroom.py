"""Dispatch width from the spawn gate's own counters.

``config.parallel.max_lanes`` was a second concurrency authority beside the
real one: while it was configured it kept refusing the epic advance at 10 live
workers against a cap of 3, on a machine whose actual binding caps (fleet
``max_live``, per-provider ``lanes``) had room. Width now derives from the
gate's own functions - the same ones ``fno agents top`` and
``advance --explain`` read - so no surface can disagree with the refusal that
follows it.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno.backlog.advance import _spawn_headroom


class _Agents:
    def __init__(self, max_live: int, limits: dict) -> None:
        self.max_live = max_live
        self.provider_limits = limits


class _Settings:
    def __init__(self, agents: _Agents) -> None:
        self.agents = agents


def _wire(
    monkeypatch: pytest.MonkeyPatch,
    *,
    max_live: int = 30,
    slots: int = 0,
    limits: dict | None = None,
    live: dict | None = None,
    cap_fn=None,
    fail: bool = False,
    cpu_verdict: str = "admit",
) -> None:
    limits = limits if limits is not None else {"zai": 7, "claude": None}
    live = live if live is not None else {}

    def fake_load_settings():
        if fail:
            raise RuntimeError("config unreadable")
        return _Settings(_Agents(max_live, limits))

    monkeypatch.setattr("fno.config.load_settings", fake_load_settings)
    from fno.agents import spawn_gate
    from fno.footprint import Admission

    monkeypatch.setattr(
        spawn_gate, "census", lambda: SimpleNamespace(slot_count=slots)
    )
    monkeypatch.setattr(
        spawn_gate, "provider_live_count", lambda name, counted=None: live.get(name, 0)
    )
    monkeypatch.setattr(
        spawn_gate,
        "provider_lanes_cap",
        cap_fn if cap_fn is not None else spawn_gate.provider_lanes_cap,
    )
    # x-7783: the CPU axis bounds the width; pinned admitting unless a test
    # asks for a hold, so no test reads the real machine.
    admission = Admission(
        verdict=cpu_verdict,
        axis="fleet_cpu_share",
        reason=f"test {cpu_verdict}",
        share_low=0.1,
        share_high=0.1,
        bound="exact",
        fleet_cores=1.2,
        machine_cores=6.0,
        capacity_cores=12.0,
        ceiling=0.5,
        gap=None,
        load_15m=1.0,
        backstop=480.0,
    )
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: admission)


def test_width_is_the_minimum_of_fleet_and_provider_headroom(monkeypatch):
    _wire(monkeypatch, max_live=30, slots=10, limits={"zai": 7}, live={"zai": 5})
    assert _spawn_headroom() == 2  # zai: 7 - 5, beats fleet: 30 - 10


def test_the_most_constrained_configured_provider_bounds_an_unpinned_read(monkeypatch):
    _wire(
        monkeypatch,
        max_live=30,
        slots=0,
        limits={"zai": 7, "claude": 20},
        live={"zai": 7, "claude": 4},
    )
    assert _spawn_headroom() == 0  # zai full; the grid could route anywhere


def test_a_provider_pin_reads_only_that_provider(monkeypatch):
    _wire(
        monkeypatch,
        max_live=30,
        slots=0,
        limits={"zai": 7, "anthropic": 20},
        live={"zai": 7, "anthropic": 4},
    )
    # Harness pin "claude" resolves to the vendor "anthropic" and reads that
    # budget; a vendor pin ("zai") falls through unchanged.
    assert _spawn_headroom("claude") == 16
    assert _spawn_headroom("zai") == 0


def test_an_uncapped_provider_cannot_bound_the_width(monkeypatch):
    _wire(monkeypatch, max_live=12, slots=2, limits={"zai": None}, live={"zai": 99})
    assert _spawn_headroom() == 10


def test_a_non_admit_cpu_verdict_zeroes_the_width(monkeypatch):
    """x-7783 AC10: the gate would queue (hold) or refuse (undecidable) every
    spawn this width dispatches, so the drain returns 0 and names the axis
    instead of manufacturing N queued spawns."""
    _wire(monkeypatch, max_live=30, slots=0, cpu_verdict="hold")
    assert _spawn_headroom() == 0


def test_zero_headroom_means_full_not_error(monkeypatch):
    _wire(monkeypatch, max_live=30, slots=30, limits={})
    assert _spawn_headroom() == 0


def test_an_unreadable_reading_degrades_to_one_lane_loudly(monkeypatch, caplog):
    _wire(monkeypatch, fail=True)
    assert _spawn_headroom() == 1


def test_parallel_max_lanes_warns_once_and_is_ignored(monkeypatch):
    """The retired key parses, prints one deprecation line, and no gate reads it.

    The measured failure this retires: 10 live workers refused at a configured
    cap of 3 while the real caps had room.
    """
    from fno.config import SettingsModel, _DEPRECATED_WARNED

    _DEPRECATED_WARNED.discard("parallel.max_lanes")
    SettingsModel.model_validate({"parallel": {"max_lanes": 3}})
    assert "parallel.max_lanes" in _DEPRECATED_WARNED

    # Once per process: a second load does not warn again.
    warned_before = len(_DEPRECATED_WARNED)
    SettingsModel.model_validate({"parallel": {"max_lanes": 3}})
    assert len(_DEPRECATED_WARNED) == warned_before


def test_a_harness_pin_reads_the_vendor_keyed_table(monkeypatch):
    """`--provider` resolves on the HARNESS axis; provider_limits is keyed by
    VENDOR. Pinning codex must read the openai budget, not miss on `codex`
    and silently drop the one cap that binds."""
    _wire(monkeypatch, max_live=30, slots=0, limits={"openai": 7}, live={"openai": 7})
    assert _spawn_headroom("codex") == 0
    assert _spawn_headroom("claude") == 30  # claude budget absent: fleet only


def test_an_unpinned_read_names_the_binding_provider(monkeypatch):
    _wire(monkeypatch, max_live=30, slots=0, limits={"openai": 7, "zai": 9},
          live={"openai": 7, "zai": 1})
    from fno.backlog.advance import _binding_provider

    assert _binding_provider() == "openai"  # 0 remaining beats zai's 8
    assert _spawn_headroom() == 0


# ---------------------------------------------------------------------------
# Per-child lanes (x-fa3a): the budget + verdict the drain and explain share
# ---------------------------------------------------------------------------


def test_a_child_is_priced_by_its_own_lane_not_the_binding_provider(monkeypatch):
    """x-fa3a AC1-HP: zai (the only configured cap) sits full, the fleet has
    room, and the child's dispatch settles on uncapped anthropic: it passes."""
    from fno.backlog import advance as adv

    _wire(monkeypatch, max_live=30, slots=5, limits={"zai": 20}, live={"zai": 20})
    monkeypatch.setattr(adv, "_child_lane_vendor", lambda child, **k: "anthropic")
    budget = adv._spawn_budget()
    assert (budget.fleet, budget.vendor_remaining) == (25, {"zai": 0})
    assert adv._lane_cap_verdict({"id": "x-bp"}, budget, total=0) == (False, "anthropic", None)


def test_a_full_lane_refuses_only_its_own_children(monkeypatch):
    """x-fa3a AC2-ERR: zai full refuses a zai child by name while a later
    anthropic child in the same pass still dispatches."""
    from fno.backlog import advance as adv

    _wire(monkeypatch, max_live=30, slots=0, limits={"zai": 20}, live={"zai": 20})
    monkeypatch.setattr(adv, "_child_lane_vendor", lambda child, **k: child["vendor"])
    budget = adv._spawn_budget()
    assert adv._lane_cap_verdict({"id": "x-z", "vendor": "zai"}, budget, total=0) == (True, "zai", 0)
    assert adv._lane_cap_verdict({"id": "x-a", "vendor": "anthropic"}, budget, total=0)[0] is False


def test_an_unresolvable_lane_keeps_the_binding_provider_rule(monkeypatch):
    """x-fa3a AC3-EDGE: a child whose lane cannot be resolved is bounded by
    the most constrained CONFIGURED provider, exactly as before x-fa3a."""
    from fno.backlog import advance as adv

    _wire(monkeypatch, max_live=30, slots=0, limits={"zai": 20}, live={"zai": 20})
    monkeypatch.setattr(adv, "_child_lane_vendor", lambda child, **k: None)
    budget = adv._spawn_budget()
    assert (budget.binding, budget.binding_remaining) == ("zai", 0)
    assert adv._lane_cap_verdict({"id": "x-u"}, budget, total=0) == (True, None, 0)


def test_one_free_lane_serves_the_first_child_on_it_only(monkeypatch):
    """x-fa3a AC3-EDGE: with one zai lane free, the first zai child fills and
    the next zai child drops with the lane named and no headroom left."""
    from fno.backlog import advance as adv

    _wire(monkeypatch, max_live=30, slots=0, limits={"zai": 20}, live={"zai": 19})
    monkeypatch.setattr(adv, "_child_lane_vendor", lambda child, **k: "zai")
    budget = adv._spawn_budget()
    assert adv._lane_cap_verdict({"id": "x-1"}, budget, total=0)[0] is False
    budget.dispatched_by_vendor["zai"] = 1
    assert adv._lane_cap_verdict({"id": "x-2"}, budget, total=1) == (True, "zai", 0)


class _ProfiledAgents:
    def __init__(self, profiles=None, defaults=None):
        self.profiles = profiles or {}
        self.defaults = defaults


def _wire_profiles(monkeypatch, profiles=None, defaults=None):
    import fno.config as _config

    monkeypatch.setattr(
        _config, "load_settings",
        lambda: _Settings(_ProfiledAgents(profiles, defaults)),
    )


def _record_vendor(monkeypatch):
    """Stub resolve_lane_vendor at its module, recording (argv, harness)."""
    from fno.agents import spawn_defaults
    calls = []

    def fake(argv, env=None, *, harness=None):
        calls.append((list(argv), harness))
        return f"vendor:{harness}:{argv[-1] if argv else 'bare'}"

    monkeypatch.setattr(spawn_defaults, "resolve_lane_vendor", fake)
    return calls


def _boom(node):
    raise RuntimeError("unanswerable")


def test_child_lane_resolution_pin_then_grid_then_verb_profile(monkeypatch):
    """x-fa3a: the helper mirrors the spawn seam - pin > grid pick (priced
    through its route) > the verb profile's own lane."""
    from fno.backlog import advance as adv

    calls = _record_vendor(monkeypatch)
    assert adv._child_lane_vendor({"id": "x"}, model=None, provider="claude") == "vendor:claude:bare"
    assert calls == [([], "claude")]

    calls.clear()
    monkeypatch.setattr(adv, "_node_effective_verb", lambda node: "target")
    monkeypatch.setattr(
        adv, "_grid_lane_for",
        lambda node, *, model, provider, verb: ("codex", "m", "openai/gpt", None, None),
    )
    got = adv._child_lane_vendor({"id": "x"}, model=None, provider=None)
    assert got == "vendor:codex:openai/gpt"
    assert calls == [(["fno", "--route", "openai/gpt"], "codex")]

    calls.clear()
    monkeypatch.setattr(
        adv, "_grid_lane_for",
        lambda node, *, model, provider, verb: (None, None, None, None, "grid=unarmed"),
    )
    monkeypatch.setattr(adv, "_node_effective_verb", lambda node: "blueprint")
    _wire_profiles(
        monkeypatch,
        profiles={"blueprint": SimpleNamespace(route="", model="opus", provider="")},
    )
    assert adv._child_lane_vendor({"id": "x"}, model=None, provider=None) == "vendor:None:opus"
    assert calls == [(["fno", "--model", "opus"], None)]


def test_an_unresolvable_child_lane_returns_none(monkeypatch):
    from fno.backlog import advance as adv

    monkeypatch.setattr(adv, "_node_effective_verb", _boom)
    assert adv._child_lane_vendor({"id": "x"}, model=None, provider=None) is None


def test_a_silent_resolver_never_miskeys_the_vendor_table(monkeypatch):
    """No overlay opinion: only a raw vendor pin scopes; a harness pin falls
    back to the configured caps (budget) / the binding cap (child lane)."""
    from fno.agents import spawn_defaults
    from fno.backlog import advance as adv

    _wire(monkeypatch, max_live=30, slots=0, limits={"zai": 20}, live={"zai": 20})
    monkeypatch.setattr(
        spawn_defaults, "resolve_lane_vendor", lambda argv, env=None, *, harness=None: None
    )
    # A harness pin with no vendor opinion keeps the configured caps binding.
    assert adv._spawn_headroom("claude") == 0
    assert adv._spawn_headroom("zai") == 0  # a raw vendor pin still scopes
    assert adv._child_lane_vendor({"id": "x"}, model=None, provider="claude") is None
    assert adv._child_lane_vendor({"id": "x"}, model=None, provider="zai") == "zai"


def test_a_fleet_full_pass_prices_no_child_lane(monkeypatch):
    """The fleet bound fires before the vendor resolution: a full fleet never
    pays a grid read per refused child."""
    from fno.backlog import advance as adv

    _wire(monkeypatch, max_live=30, slots=30, limits={})
    monkeypatch.setattr(adv, "_child_lane_vendor", _boom)
    budget = adv._spawn_budget()
    assert adv._lane_cap_verdict({"id": "x"}, budget, total=0) == (True, None, 0)


def test_a_profile_with_no_routing_names_no_lane(monkeypatch):
    """Nothing the spawn would inherit names a lane, so the child keeps the
    binding-provider rule instead of pricing against the caller's own env."""
    from fno.backlog import advance as adv

    _record_vendor(monkeypatch)
    monkeypatch.setattr(adv, "_node_effective_verb", lambda node: None)
    monkeypatch.setattr(
        adv, "_grid_lane_for",
        lambda node, *, model, provider, verb: (None, None, None, None, "grid=unarmed"),
    )
    _wire_profiles(monkeypatch)
    assert adv._child_lane_vendor({"id": "x"}, model=None, provider=None) is None
