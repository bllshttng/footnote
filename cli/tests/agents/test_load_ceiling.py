"""x-7783 Change 2: the CPU axis is read from one decider, never computed twice.

The decision itself is ``cpu_admission`` in doctor_footprint, pinned against
the shared fixture in test_footprint_verb.py. What lives here is the gate's
own edge of that contract: ``_cpu_axis``'s mapping of a reading or an error
to an Admission, the 15-minute backstop input, and the platform-tolerance
edges. The hold and debounce behavior lives in test_fleet_load_governor.py.
"""
from __future__ import annotations

import pytest

from fno.agents import spawn_gate
from fno.footprint import Footprint


def _reading(
    fleet: float, measured: float, gap: str | None = None, top: list | None = None
) -> Footprint:
    return Footprint(
        sustained_cpu_cores=0.0,
        descendant_cpu_cores=0.0,
        fleet_cpu_cores=fleet,
        descendant_process_count=0,
        direct_process_count=0,
        transient_call_count=0,
        process_count=0,
        rss_gb=0.0,
        measured_cpu_cores=measured,
        top=top or [],
        unparsed_lines=0,
        attribution_gap=gap,
    )


@pytest.fixture(autouse=True)
def _quiet_edges(monkeypatch):
    """No unit test reads the real machine: the share/backstop pair is the
    stock default and the capacity is pinned."""
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (0.5, 40.0))
    monkeypatch.setattr(spawn_gate, "_load_cpus", lambda: 12)


def _pin_load15(monkeypatch, value: float):
    monkeypatch.setattr(spawn_gate.os, "getloadavg", lambda: (1.0, 1.0, value))


def test_unreadable_instrument_refuses_with_the_error_text():
    admission = spawn_gate._cpu_axis(
        (None, "footprint unavailable: ps unavailable: timed out after 5.0s")
    )
    assert admission.verdict == "refuse"
    assert admission.axis == "cpu_instrument"
    assert "ps unavailable: timed out after 5.0s" in admission.reason
    assert "--force to bypass" in admission.reason


def test_backstop_refuses_on_load_15m_and_names_no_one_minute_figure(monkeypatch):
    """AC6-EDGE: a 15-minute load of 500 on 12 cpus against hard 40 refuses
    on the backstop carrying `load_15m: 500.0`, and no one-minute figure
    appears in the message."""
    _pin_load15(monkeypatch, 500.0)
    admission = spawn_gate._cpu_axis((_reading(0.1, 6.9), None))
    assert admission.verdict == "refuse"
    assert admission.axis == "load_15m"
    assert admission.load_15m == 500.0
    assert admission.backstop == 480.0
    assert "500.0" in admission.reason and "480.0" in admission.reason
    assert "1-min" not in admission.reason


def test_backstop_passes_at_141_on_the_same_box(monkeypatch):
    _pin_load15(monkeypatch, 141.0)
    admission = spawn_gate._cpu_axis((_reading(0.1, 6.9), None))
    assert admission.verdict == "admit"
    assert admission.axis == "fleet_cpu_share"


def test_disabled_backstop_passes_any_load(monkeypatch):
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (0.5, 0.0))
    _pin_load15(monkeypatch, 500.0)
    admission = spawn_gate._cpu_axis((_reading(0.1, 6.9), None))
    assert admission.verdict == "admit"


def test_unreadable_load_admits(monkeypatch):
    """LD3: unreadable load admits - the platform may have no getloadavg."""

    def boom():
        raise OSError("no loadavg here")

    monkeypatch.setattr(spawn_gate.os, "getloadavg", boom)
    admission = spawn_gate._cpu_axis((_reading(0.1, 6.9), None))
    assert admission.verdict == "admit"
    assert admission.load_15m is None


def test_gapped_reading_refuses_inside_the_interval(monkeypatch):
    gap = "3 pidless row(s) with no identity route (codex)"
    _pin_load15(monkeypatch, 45.0)
    admission = spawn_gate._cpu_axis((_reading(2.1, 7.2, gap), None))
    assert admission.verdict == "undecidable"
    assert "17.5%" in admission.reason and "60.0%" in admission.reason
    assert gap in admission.reason


def test_hold_names_the_top_holder(monkeypatch):
    """x-5f0b: the hold names who holds the cores, from the same rows the
    number was summed from. The specimen is the measured 2026-09-11 refusal:
    16 yes rows summing 513.0 %cpu out of one worktree's repro loop."""
    _pin_load15(monkeypatch, 45.0)
    top = [(32.0625, "yes > .fno/worktrees/x-b1ee/repro.out")] * 16
    admission = spawn_gate._cpu_axis((_reading(7.5, 7.5, top=top), None))
    assert admission.verdict == "hold"
    holder = "yes 16 procs 5.13 cores in .fno/worktrees/x-b1ee"
    assert f"top holder {holder}" in admission.reason
    assert admission.top_holder == holder


def test_hold_with_empty_top_names_no_holder(monkeypatch):
    """No rows in hand, no clause: the sentence is exactly what it was
    before, with no empty bracket and no doubled separator."""
    _pin_load15(monkeypatch, 45.0)
    admission = spawn_gate._cpu_axis((_reading(7.5, 7.5), None))
    assert admission.verdict == "hold"
    assert admission.top_holder is None
    assert "top holder" not in admission.reason
    assert ";;" not in admission.reason and " ;" not in admission.reason


def test_admit_reason_stays_holder_free(monkeypatch):
    """An admission needs no action, so it needs no holder; the field may
    still carry what the rows said."""
    _pin_load15(monkeypatch, 45.0)
    admission = spawn_gate._cpu_axis((_reading(0.1, 6.9, top=[(500.0, "yes")]), None))
    assert admission.verdict == "admit"
    assert "top holder" not in admission.reason


def test_retired_trigger_key_still_parses_and_defaults():
    from fno.config import AgentsBlock

    a = AgentsBlock()
    assert a.max_load_per_cpu == 8.0
    assert AgentsBlock(max_load_per_cpu=0).max_load_per_cpu == 0.0  # parses; ignored
    assert AgentsBlock(max_load_per_cpu="2.5").max_load_per_cpu == 2.5
    assert AgentsBlock(max_load_per_cpu="junk").max_load_per_cpu == 8.0
