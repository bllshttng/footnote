"""Tests for the ``fno doctor lanes`` advisor: the arm table, the dark-sensor
refusal, and the swap-total-zero fallback.

The sensor functions are injected, so no test needs macmon on PATH except the
one live smoke that skips without it.
"""

from __future__ import annotations

import json
import shutil
import subprocess
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno import doctor_lanes as dl
from fno.cli import app

runner = CliRunner()

#: Captured before any test patches it, so a test that wants the REAL cost
#: function back cannot accidentally re-pin the fake over itself.
_REAL_FLEET_COST = dl._fleet_cost


def _macmon_sample(**overrides):
    sample = {
        "cpu_usage_pct": 0.45,
        "sys_power": 7.7,
        "temp": {"cpu_temp_avg": 64.1},
        "memory": {
            "ram_total": 103079215104,  # 103 GB
            "ram_usage": 81475960832,  # 81.5 GB
            "swap_total": 0,
            "swap_usage": 0,
        },
    }
    sample.update(overrides)
    return sample


def _footprint(*, tests: int = 3, gap: str | None = None, top=()):
    """A footprint reading stand-in: only the fields the census reads."""
    return SimpleNamespace(
        fleet_cpu_cores=0.5,
        rss_gb=1.9,
        test_process_count=tests,
        attribution_gap=gap,
        top=list(top),
    )


def _rows(count: int):
    return [SimpleNamespace(status="live") for _ in range(count)]


def _pin_admission(
    monkeypatch,
    *,
    verdict: str = "admit",
    share_low: float = 0.1,
    load_15m: float | None = 6.3,
):
    """Pin the CPU axis at the seam the arm reads, so no test reads the real
    machine (x-7783)."""
    from fno.agents import spawn_gate
    from fno.footprint import Admission

    admission = Admission(
        verdict=verdict,
        axis="fleet_cpu_share",
        reason=f"test {verdict}",
        share_low=share_low,
        share_high=share_low,
        bound="exact",
        fleet_cores=share_low * 12.0,
        machine_cores=share_low * 12.0,
        capacity_cores=12.0,
        ceiling=0.5,
        gap=None,
        load_15m=load_15m,
        backstop=480.0,
    )
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: admission)
    monkeypatch.setattr(
        dl.os, "getloadavg", lambda: (1.2, 1.1, load_15m or 1.0), raising=False
    )


def _healthy_reading(monkeypatch, sample=None):
    """Everything measured: macmon answering, and memory_pressure answering
    too - the sample's swap_total is 0, so on this shape memory_pressure IS
    the memory arm (the specifying machine's real state)."""
    _pin_admission(monkeypatch)
    monkeypatch.setattr(
        dl, "read_macmon", lambda **k: (sample or _macmon_sample(), None)
    )
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (0.84, None))
    monkeypatch.setattr(
        dl,
        "_fleet_snapshot",
        lambda: (_footprint(), _rows(6), None, 421),
    )
    monkeypatch.setattr(
        dl,
        "_fleet_cost",
        lambda *_a: (
            0.08,
            0.31,
            6,
            "measured from the live roster's attributed footprint",
        ),
    )
    monkeypatch.setattr(
        "fno.agents.court.gather_court",
        lambda rows=None: {
            "crowns": [],
            "conflicts": [],
            "summary": {"total": 2, "disagreements": 0, "unknowns": 0},
        },
    )


def test_healthy_machine_answers_with_per_arm_readings(monkeypatch) -> None:
    _healthy_reading(monkeypatch)
    reading = dl.read_lanes()
    assert not reading.refused
    assert reading.lane_count is not None and reading.lane_count >= 0
    by_name = {a.name: a for a in reading.arms}
    assert by_name["whole-machine cpu"].state == dl.MEASURED
    assert by_name["whole-machine cpu"].source == "macmon cpu_usage_pct"
    # swap_total is 0 in the sample: the memory arm MUST NOT read swap 0 as
    # headroom, it must name memory_pressure as its source.
    assert by_name["memory"].state == dl.MEASURED
    assert "memory_pressure" in by_name["memory"].source
    assert by_name["power and thermals"].state == dl.MEASURED
    assert by_name["cpu admission"].state == dl.MEASURED


def test_swap_total_zero_never_reads_swap_zero_as_headroom(monkeypatch) -> None:
    """The measured case that forced the rule: 81.5 of 103 GB used, swap 0
    because NO swap file exists. The memory arm falls to memory_pressure and
    says so."""
    _healthy_reading(monkeypatch, _macmon_sample())
    reading = dl.read_lanes()
    mem = reading.arm("memory")
    assert mem.state == dl.MEASURED
    assert "no swap file" in mem.source
    assert "memory_pressure" in mem.source
    # 84% free of 103 GB is ~86 GB available, not the swap-derived 0.
    assert mem.value["free_fraction"] == 0.84
    assert mem.value["available_gb"] == pytest.approx(86.6, abs=0.2)


def test_memory_falls_back_when_memory_pressure_also_dark(monkeypatch) -> None:
    """Both memory sources unreadable: the arm is DARK with both reasons, and
    the verb refuses rather than guessing."""
    _pin_admission(monkeypatch)
    monkeypatch.setattr(
        dl,
        "read_macmon",
        lambda **k: (None, "macmon not on PATH (brew install macmon; Apple Silicon only)"),
    )
    monkeypatch.setattr(
        dl, "read_memory_pressure", lambda **k: (None, "no free-percentage line")
    )
    reading = dl.read_lanes()
    assert reading.refused
    mem = reading.arm("memory")
    assert mem.state == dl.DARK
    assert "memory_pressure" in mem.reason
    assert reading.arm("whole-machine cpu").state == dl.DARK
    assert "brew install macmon" in reading.arm("whole-machine cpu").reason


def test_dark_arms_are_named_and_working_arms_survive(monkeypatch) -> None:
    _pin_admission(monkeypatch)
    monkeypatch.setattr(
        dl, "read_macmon", lambda **k: (None, "macmon not on PATH")
    )
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (0.84, None))
    reading = dl.read_lanes()
    assert reading.refused
    # CPU dark (macmon gone) but memory measured via the fallback: the refusal
    # names BOTH facts.
    assert reading.arm("whole-machine cpu").state == dl.DARK
    assert reading.arm("memory").state == dl.MEASURED
    assert "still working" in reading.refusal_reason
    assert "memory" in reading.refusal_reason


def test_macmon_timeout_is_dark_never_headroom(monkeypatch) -> None:
    """A hanging macmon: the read is bounded, the arm is dark, and no number
    is printed on the strength of a sensor that never answered."""
    _pin_admission(monkeypatch)
    monkeypatch.setattr(
        dl, "read_macmon", lambda **k: (None, "macmon produced no sample within 5s")
    )
    monkeypatch.setattr(
        dl, "read_memory_pressure", lambda **k: (None, "timed out")
    )
    reading = dl.read_lanes()
    assert reading.refused
    assert "no sample" in reading.arm("whole-machine cpu").reason


def test_a_hold_or_refusal_verdict_caps_the_answer_at_zero(monkeypatch) -> None:
    sample = _macmon_sample(
        cpu_usage_pct=0.05,
        memory={
            "ram_total": 103079215104,
            "ram_usage": 10307921510,  # 90% free
            "swap_total": 0,
            "swap_usage": 0,
        },
    )
    _pin_admission(monkeypatch, verdict="hold", share_low=0.6)
    monkeypatch.setattr(dl, "read_macmon", lambda **k: (sample, None))
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (0.9, None))
    reading = dl.read_lanes()
    assert not reading.refused
    assert reading.lane_count == 0
    assert "capped at 0" in reading.cost_source
    assert "cpu admission hold" in reading.cost_source


def test_per_lane_cost_is_measured_not_assumed(monkeypatch) -> None:
    _healthy_reading(monkeypatch)
    reading = dl.read_lanes()
    assert reading.per_lane_cpu_cores == 0.08
    assert reading.per_lane_mem_gb == 0.31
    assert "6 live row(s)" in reading.cost_source


def test_memory_pressure_output_is_parsed(monkeypatch) -> None:
    """The real ``memory_pressure`` line shape, from the specifying machine."""

    class FakeExpired(subprocess.SubprocessError):
        stdout = (
            b"The system has 103079215104 bytes.\n"
            b"System-wide memory free percentage: 87%\n"
        )

    def fake_run(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd="memory_pressure", timeout=5.0, output=FakeExpired.stdout)

    monkeypatch.setattr(dl.subprocess, "run", fake_run)
    free, reason = dl.read_memory_pressure()
    assert free == 0.87
    assert reason is None


def test_json_payload_carries_per_arm_state(monkeypatch) -> None:
    _healthy_reading(monkeypatch)
    result = runner.invoke(app, ["doctor", "lanes", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["lane_count"] is not None
    states = {a["name"]: a["state"] for a in payload["arms"]}
    assert set(states.values()) == {dl.MEASURED}
    dark = {a["name"]: a for a in payload["arms"] if a["state"] == dl.DARK}
    assert all(a["reason"] for a in dark.values())


def test_refusal_exits_nonzero_and_prints_no_number(monkeypatch) -> None:
    _pin_admission(monkeypatch)
    monkeypatch.setattr(dl, "read_macmon", lambda **k: (None, "macmon not on PATH"))
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (None, "unreadable"))
    result = runner.invoke(app, ["doctor", "lanes"])
    assert result.exit_code == 3, result.output
    assert "REFUSED" in result.stdout
    assert "dark" in result.stdout
    # No lane number anywhere in the human output.
    assert "more fit" not in result.stdout


def test_a_macmon_fraction_passes_through_unrescaled(monkeypatch) -> None:
    """macmon's measured contract is a 0-1 fraction, and the arm does not
    guess about hypothetical percent-spelling builds: 45 means 45x the
    machine there, so no normalization sits in the way."""
    _pin_admission(monkeypatch)
    sample = _macmon_sample(cpu_usage_pct=0.45)
    monkeypatch.setattr(dl, "read_macmon", lambda **k: (sample, None))
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (0.84, None))
    reading = dl.read_lanes()
    assert reading.arm("whole-machine cpu").value["busy_fraction"] == 0.45


def test_the_machine_cpu_arm_quotes_the_machine_census(monkeypatch) -> None:
    """x-d6ad AC10: the lanes arm quotes the same machine-wide census the
    control-plane arm reads, taken from the one footprint read the fleet
    census already made."""
    _healthy_reading(monkeypatch)
    footprint = SimpleNamespace(
        fleet_cpu_cores=0.5,
        rss_gb=1.9,
        test_process_count=0,
        attribution_gap=None,
        top=[],
        runnable_count=66,
        machine_process_count=1010,
    )
    monkeypatch.setattr(dl, "_fleet_snapshot", lambda: (footprint, _rows(6), None, 421))
    reading = dl.read_lanes()
    value = reading.arm("whole-machine cpu").value
    assert value["runnable"] == 66
    assert value["processes"] == 1010


@pytest.mark.skipif(
    shutil.which("macmon") is None,
    reason="macmon not on PATH; the live smoke needs the real sensor",
)
def test_live_macmon_smoke_answers_on_a_healthy_machine(monkeypatch) -> None:
    """The plan's own acceptance: macmon present, healthy machine -> a lane
    number and a per-arm reading list, from the REAL sensor."""
    _pin_admission(monkeypatch)
    result = runner.invoke(app, ["doctor", "lanes", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["lane_count"] is not None
    states = {a["name"]: a["state"] for a in payload["arms"]}
    assert states["whole-machine cpu"] == dl.MEASURED
    assert states["memory"] == dl.MEASURED


def test_ac2_hp_census_counts_add_up_and_the_cost_is_measured(monkeypatch) -> None:
    """AC2-HP: kings plus workers equals the roster rows, because both come
    from ONE rows list. The per-lane cost reads measured, never seed."""
    _healthy_reading(monkeypatch)

    reading = dl.read_lanes()

    census = reading.census
    assert census["roster_rows"] == 6
    assert census["kings"] == 2
    assert census["workers"] == 4
    assert census["kings"] + census["workers"] == census["roster_rows"]
    assert census["tests"] == 3
    assert census["read_ms"] == 421
    assert "measured" in reading.cost_source
    assert "seed" not in reading.cost_source


def test_ac2_edge_unreadable_registry_nulls_the_counts_and_keeps_the_seed(
    monkeypatch,
) -> None:
    """AC2-EDGE: an unreadable registry must never render as an empty fleet.
    The counts are null and the cost falls back to the documented seed with
    its reason named."""
    _healthy_reading(monkeypatch)
    monkeypatch.setattr(dl, "_fleet_snapshot", lambda: (_footprint(), None, "registry unreadable", 12))
    monkeypatch.setattr(dl, "_fleet_cost", _REAL_FLEET_COST)

    reading = dl.read_lanes()

    census = reading.census
    assert census["roster_rows"] is None
    assert census["kings"] is None
    assert census["workers"] is None
    assert reading.cost_source == "seed (no live roster rows to measure)"
    text = dl.render(reading)
    assert "unknown" in text
    assert "roster: registry unreadable - the counts above are unread" in text


def test_the_census_renders_on_a_refusal_too(monkeypatch) -> None:
    """A refused lane number is exactly when a person most wants the census."""
    _pin_admission(monkeypatch)
    monkeypatch.setattr(dl, "read_macmon", lambda **k: (None, "macmon not on PATH"))
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (None, "unreadable"))
    monkeypatch.setattr(dl, "_fleet_snapshot", lambda: (_footprint(), _rows(4), None, 30))
    monkeypatch.setattr(
        "fno.agents.court.gather_court",
        lambda rows=None: {"conflicts": [], "summary": {"total": 1}},
    )

    reading = dl.read_lanes()

    assert reading.refused
    assert reading.census["roster_rows"] == 4
    assert "court: 1 king(s), 3 worker(s)" in dl.render(reading)


def test_the_attribution_gap_rides_its_own_line_never_the_counts(
    monkeypatch,
) -> None:
    """x-e040: the gap is a process-to-row failure, so it cannot corrupt a row
    count. It qualifies the CPU reading and is never folded into the census."""
    _healthy_reading(monkeypatch)
    gap = "11 pidless row(s) with no identity route (codex)"
    monkeypatch.setattr(
        dl, "_fleet_snapshot", lambda: (_footprint(gap=gap), _rows(6), None, 40)
    )

    text = dl.render(dl.read_lanes())

    assert gap in text
    assert "undercount, not headroom" in text
    assert "court: 2 king(s), 4 worker(s)" in text


def test_a_king_conflict_is_rendered_because_a_bare_count_hides_it(
    monkeypatch,
) -> None:
    _healthy_reading(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.court.gather_court",
        lambda rows=None: {
            "conflicts": [{"scope": "node:x-1", "holders": ["a", "b"]}],
            "summary": {"total": 2},
        },
    )

    reading = dl.read_lanes()

    assert reading.census["king_conflicts"] == 1
    assert "court conflicts: 1 scope(s)" in dl.render(reading)


def test_an_unreadable_court_nulls_the_crowns_rather_than_reporting_none(
    monkeypatch,
) -> None:
    """gather_court nulls its summary on an unreadable registry. Reading that
    null as zero kings would report a kingless fleet from a read that saw
    nothing."""
    _healthy_reading(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.court.gather_court",
        lambda rows=None: {"conflicts": None, "summary": {"total": None}},
    )

    census = dl.read_lanes().census

    assert census["roster_rows"] == 6
    assert census["kings"] is None
    assert census["workers"] is None


def test_json_payload_carries_the_census(monkeypatch) -> None:
    _healthy_reading(monkeypatch)
    result = runner.invoke(app, ["doctor", "lanes", "--json"])
    assert result.exit_code == 0, result.output
    census = json.loads(result.stdout)["census"]
    assert census["kings"] == 2
    assert census["workers"] == 4
    assert census["tests"] == 3


def test_the_cpu_admission_arm_names_the_whole_vocabulary(monkeypatch) -> None:
    """x-7783 AC12 (carrying x-aeab's rule): the panel cannot label what the
    arm never emits. The interval bounds, the ceiling, the backstop and its
    15-minute figure, and the 1m/5m trend ride on the value object."""
    from fno.agents import spawn_gate
    from fno.footprint import Admission

    admission = Admission(
        verdict="undecidable",
        axis="fleet_cpu_share",
        reason="spawn-gate: cannot decide",
        share_low=0.175,
        share_high=0.6,
        bound="upper",
        fleet_cores=2.1,
        machine_cores=7.2,
        capacity_cores=12.0,
        ceiling=0.5,
        gap="3 pidless row(s)",
        load_15m=143.8,
        backstop=480.0,
    )
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: admission)
    monkeypatch.setattr(dl.os, "getloadavg", lambda: (184.9, 122.3, 143.8))

    arm = {a.name: a for a in dl.read_lanes().arms}["cpu admission"]

    assert arm.value["fleet_cores"] == 2.1
    assert arm.value["share_low"] == 0.175
    assert arm.value["share_high"] == 0.6
    assert arm.value["bound"] == "upper"
    assert arm.value["ceiling"] == 0.5
    assert arm.value["verdict"] == "undecidable"
    assert arm.value["load_15m"] == 143.8
    assert arm.value["backstop"] == 480.0
    assert arm.value["load_1m"] == 184.9
    assert arm.value["load_5m"] == 122.3


def _instrument_admission():
    from fno.footprint import Admission

    return Admission(
        verdict="refuse",
        axis="cpu_instrument",
        reason=(
            "spawn-gate: the CPU instrument is unreadable "
            "(footprint unavailable: ps unavailable: timed out after 5.0s); "
            "refusing to spawn (--force to bypass)"
        ),
        share_low=0.0,
        share_high=0.0,
        bound="exact",
        fleet_cores=0.0,
        machine_cores=0.0,
        capacity_cores=0.0,
        ceiling=0.0,
        gap=None,
        load_15m=None,
        backstop=0.0,
    )


def test_the_cpu_admission_arm_goes_dark_when_the_instrument_never_answered(
    monkeypatch,
) -> None:
    """Zeros the instrument never measured are not a reading. The
    arm carries the admission's own words as its reason instead."""
    from fno.agents import spawn_gate

    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: _instrument_admission())
    arm = dl._cpu_admission_arm()
    assert arm.state == dl.DARK
    assert arm.value is None
    assert "ps unavailable: timed out after 5.0s" in arm.reason


def test_a_dark_cpu_admission_arm_never_refuses_the_lane_answer(monkeypatch) -> None:
    """read_lanes gates on the whole-machine cpu and memory arms only; the
    admission arm going dark leaves the lane ANSWER standing - but a dark
    sensor is never headroom, so the count caps at 0 rather than advising
    lanes on top of an axis that never answered."""
    from fno.agents import spawn_gate

    _healthy_reading(monkeypatch)
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: _instrument_admission())
    reading = dl.read_lanes()
    assert not reading.refused
    assert reading.lane_count == 0
    assert reading.refusal_reason == ""
    arm = reading.arm("cpu admission")
    assert arm.state == dl.DARK
    assert "capped at 0" in reading.cost_source


def test_the_census_carries_top_consumers_from_the_ps_read(monkeypatch) -> None:
    """x-aeab: name what is saturating the box, from rows a read already
    performed - never a second instrument."""
    _healthy_reading(monkeypatch)
    monkeypatch.setattr(
        dl,
        "_fleet_snapshot",
        lambda: (
            _footprint(
                top=[
                    (210.0, "/usr/local/bin/python3 fno-py doctor test"),
                    (45.0, "fno-agents-worker --worktree .fno/worktrees/x-b1ee"),
                ]
            ),
            _rows(6),
            None,
            421,
        ),
    )

    census = dl.read_lanes().census

    top = census["top_consumers"]
    assert top[0]["name"] == "python3"
    assert top[0]["procs"] == 1
    assert top[0]["cpu_pct"] == 210.0
    assert census["top_consumers"][1]["worktree"] == ".fno/worktrees/x-b1ee"
    assert census["top_consumers"][1]["worktree_procs"] == 1


def test_a_dark_footprint_leaves_top_consumers_none(monkeypatch) -> None:
    """A reading that failed names so; the panel prints a degrade, never a
    fabricated consumer list."""
    _healthy_reading(monkeypatch)
    monkeypatch.setattr(
        dl, "_fleet_snapshot", lambda: (None, _rows(6), "ps unavailable", 5)
    )

    census = dl.read_lanes().census

    assert census["top_consumers"] is None
