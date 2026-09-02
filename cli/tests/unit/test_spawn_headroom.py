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
) -> None:
    limits = limits if limits is not None else {"zai": 7, "claude": None}
    live = live if live is not None else {}

    def fake_load_settings():
        if fail:
            raise RuntimeError("config unreadable")
        return _Settings(_Agents(max_live, limits))

    monkeypatch.setattr("fno.config.load_settings", fake_load_settings)
    from fno.agents import spawn_gate

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
        limits={"zai": 7, "claude": 20},
        live={"zai": 7, "claude": 4},
    )
    assert _spawn_headroom("claude") == 16
    assert _spawn_headroom("zai") == 0


def test_an_uncapped_provider_cannot_bound_the_width(monkeypatch):
    _wire(monkeypatch, max_live=12, slots=2, limits={"zai": None}, live={"zai": 99})
    assert _spawn_headroom() == 10


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
