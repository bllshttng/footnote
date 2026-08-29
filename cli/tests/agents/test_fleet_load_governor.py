"""x-7c0f: the spawn gate governs on FLEET-attributed CPU, not machine load.

The measured defect, twice: the gate refused a dispatch at 1-min load 127.6
against a ceiling of 96.0 while the SAME refusal printed that footprint
attributed 0.79 of 12.00 cores (6.6% of capacity) to the fleet. On
2026-08-29 the three largest CPU consumers on that box were desktop apps a
worker cannot influence, and a single unscoped ripgrep was worth roughly 195
load points. Load average counts blocked processes, so it is not a CPU
measure and it is not attributable to anyone.

The gate keeps a guard in both directions. `max_load_per_cpu` is now the
load at which the gate STOPS TRUSTING LOAD and consults attribution;
`max_fleet_cpu_share` decides; `hard_max_load_per_cpu` is the absolute
machine backstop that refuses regardless of whose load it is.
"""
from __future__ import annotations

import pytest

from fno.agents import spawn_gate

TRIGGER = 8.0
SHARE = 0.5
HARD = 40.0


@pytest.fixture(autouse=True)
def _fixed_cpus(monkeypatch):
    monkeypatch.setattr(spawn_gate.os, "cpu_count", lambda: 12)
    if hasattr(spawn_gate.os, "process_cpu_count"):
        monkeypatch.setattr(spawn_gate.os, "process_cpu_count", lambda: 12)


def _load(monkeypatch, load1: float):
    monkeypatch.setattr(spawn_gate.os, "getloadavg", lambda: (load1, 0.0, 0.0))


def _fleet(monkeypatch, reading):
    """Seam for the footprint attribution: (fleet_cores, capacity) or None."""
    monkeypatch.setattr(spawn_gate, "_fleet_cpu_reading", lambda: reading)


def _check(**kw):
    kw.setdefault("max_load_per_cpu", TRIGGER)
    kw.setdefault("max_fleet_cpu_share", SHARE)
    kw.setdefault("hard_max_load_per_cpu", HARD)
    return spawn_gate._check_load_ceiling(**kw)


def test_foreign_load_admits_the_spawn(monkeypatch, capsys):
    """THE REGRESSION THIS NODE EXISTS FOR: the exact refusal, now admitted.

    Load 127.6 over a 96.0 trigger, fleet holding 0.79 of 12 cores. The load
    is real and it is not ours, so the spawn proceeds.
    """
    _load(monkeypatch, 127.6)
    _fleet(monkeypatch, (0.79, 12.0))
    _check()  # no raise
    err = capsys.readouterr().err
    assert "0.79" in err and "not attributed to the fleet" in err


def test_fleet_owned_load_still_refuses(monkeypatch, capsys):
    """The guard survives: our own fleet over the share ceiling is refused."""
    _load(monkeypatch, 127.6)
    _fleet(monkeypatch, (9.0, 12.0))  # 75% of capacity, over the 50% share
    with pytest.raises(spawn_gate.GateRefused) as ei:
        _check()
    assert ei.value.code == spawn_gate.EXIT_LOAD_REFUSED
    err = capsys.readouterr().err
    assert "9.00" in err and "12.00" in err and "--force" in err


def test_absolute_backstop_refuses_regardless_of_attribution(monkeypatch):
    """The peer king's constraint: a thrashing box refuses even foreign load.

    Pure fleet-share would admit onto a box at load 600 because the fleet
    owns almost none of it. The backstop is what stops that.
    """
    _load(monkeypatch, 600.0)  # over 40 x 12 = 480
    _fleet(monkeypatch, (0.1, 12.0))  # fleet owns essentially nothing
    with pytest.raises(spawn_gate.GateRefused):
        _check()


def test_under_trigger_never_probes_footprint(monkeypatch):
    """The common path costs no subprocess: below the trigger we never ask."""
    _load(monkeypatch, 24.0)

    def boom():
        raise AssertionError("footprint probed below the trigger")

    monkeypatch.setattr(spawn_gate, "_fleet_cpu_reading", boom)
    _check()  # no raise


def test_unreadable_footprint_fails_closed(monkeypatch, capsys):
    """Above the trigger with no attribution we do not know whose load it is.

    Admitting here is the failure mode x-e040 already produced once: the
    sensor goes blind under exactly the load it measures and its silence
    reads to a caller as headroom.
    """
    _load(monkeypatch, 127.6)
    _fleet(monkeypatch, None)
    with pytest.raises(spawn_gate.GateRefused):
        _check()
    assert "attribution unavailable" in capsys.readouterr().err


def test_disabled_trigger_never_fires(monkeypatch):
    _load(monkeypatch, 309.0)
    _fleet(monkeypatch, (11.9, 12.0))
    _check(max_load_per_cpu=0)
    _check(max_load_per_cpu=-1)


def test_unreadable_load_skips(monkeypatch, capsys):
    def boom():
        raise OSError("no loadavg here")

    monkeypatch.setattr(spawn_gate.os, "getloadavg", boom)
    _check()
    assert "skipping the load check" in capsys.readouterr().err


def test_config_defaults_and_coercion():
    from fno.config import AgentsBlock

    a = AgentsBlock()
    assert a.max_fleet_cpu_share == 0.5
    assert a.hard_max_load_per_cpu == 40.0
    assert AgentsBlock(max_fleet_cpu_share="0.25").max_fleet_cpu_share == 0.25
    assert AgentsBlock(max_fleet_cpu_share="junk").max_fleet_cpu_share == 0.5
    assert AgentsBlock(hard_max_load_per_cpu="junk").hard_max_load_per_cpu == 40.0


def test_backstop_stays_well_above_the_trigger():
    """A backstop at or below the trigger would silently restore the defect."""
    from fno.config import AgentsBlock

    a = AgentsBlock()
    assert a.hard_max_load_per_cpu > a.max_load_per_cpu * 4


def test_rust_probe_budget_exceeds_the_python_measurement_budget():
    """The two runtimes must not disagree about admission on a slow box.

    Both gates refuse when fleet attribution is unreadable, so whichever one
    gives up first refuses first. Python calls `cause_reading` IN PROCESS and
    spends its whole budget measuring. Rust runs the same reading as a
    subprocess, so its budget must also cover spawning the CLI and importing
    it. Equal numbers therefore do NOT mean equal behaviour: they make the
    Rust gate time out first, and since this node a timeout REFUSES rather
    than merely losing the explanation.

    Read from the Rust source because there is no shared constant to import.
    A cheap string read is worth more than an untested comment, and this
    fails loudly if either budget moves.
    """
    import inspect
    import re
    from pathlib import Path

    from fno import doctor_footprint

    python_budget = inspect.signature(
        doctor_footprint.cause_reading
    ).parameters["timeout"].default
    assert python_budget == 5.0

    rust = Path(__file__).resolve().parents[3] / "crates/fno-agents/src/spawn_gate.rs"
    source = rust.read_text()
    match = re.search(
        r"const FOOTPRINT_PROBE_BUDGET: Duration = Duration::from_secs\((\d+)\)",
        source,
    )
    assert match, "FOOTPRINT_PROBE_BUDGET missing or renamed in the Rust gate"
    rust_budget = int(match.group(1))

    assert rust_budget > python_budget, (
        f"Rust probe budget {rust_budget}s must exceed the Python measurement "
        f"budget {python_budget}s, or the Rust gate refuses on a loaded box "
        f"where the Python gate admits"
    )
