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


def _pin_load(monkeypatch, *, status: str = "within", load: float = 6.3):
    from fno import doctor_footprint

    snapshot = SimpleNamespace(
        load_1m=load,
        max_load_per_cpu=8.0,
        load_ceiling=96.0,
        load_cpu_count=12,
        spawn_load_status=status,
    )
    monkeypatch.setattr(doctor_footprint, "_spawn_load_snapshot", lambda: snapshot)
    monkeypatch.setattr(doctor_footprint, "_cpu_capacity_cores", lambda: 12)


def _healthy_reading(monkeypatch, sample=None):
    """Everything measured: macmon answering, and memory_pressure answering
    too - the sample's swap_total is 0, so on this shape memory_pressure IS
    the memory arm (the specifying machine's real state)."""
    _pin_load(monkeypatch)
    monkeypatch.setattr(
        dl, "read_macmon", lambda **k: (sample or _macmon_sample(), None)
    )
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (0.84, None))
    monkeypatch.setattr(
        dl,
        "_fleet_cost",
        lambda: (0.08, 0.31, 6, "measured from the live roster's attributed footprint"),
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
    assert by_name["spawn load"].state == dl.MEASURED


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
    _pin_load(monkeypatch)
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
    _pin_load(monkeypatch)
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
    _pin_load(monkeypatch)
    monkeypatch.setattr(
        dl, "read_macmon", lambda **k: (None, "macmon produced no sample within 5s")
    )
    monkeypatch.setattr(
        dl, "read_memory_pressure", lambda **k: (None, "timed out")
    )
    reading = dl.read_lanes()
    assert reading.refused
    assert "no sample" in reading.arm("whole-machine cpu").reason


def test_a_breached_spawn_load_ceiling_caps_the_answer_at_zero(monkeypatch) -> None:
    sample = _macmon_sample(
        cpu_usage_pct=0.05,
        memory={
            "ram_total": 103079215104,
            "ram_usage": 10307921510,  # 90% free
            "swap_total": 0,
            "swap_usage": 0,
        },
    )
    _pin_load(monkeypatch, status="exceeded")
    monkeypatch.setattr(dl, "read_macmon", lambda **k: (sample, None))
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (0.9, None))
    reading = dl.read_lanes()
    assert not reading.refused
    assert reading.lane_count == 0
    assert "breached" in reading.cost_source


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
    _pin_load(monkeypatch)
    monkeypatch.setattr(dl, "read_macmon", lambda **k: (None, "macmon not on PATH"))
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (None, "unreadable"))
    result = runner.invoke(app, ["doctor", "lanes"])
    assert result.exit_code == 3, result.output
    assert "REFUSED" in result.stdout
    assert "dark" in result.stdout
    # No lane number anywhere in the human output.
    assert "more fit" not in result.stdout


def test_a_macmon_percent_spelling_normalizes(monkeypatch) -> None:
    """Some macmon builds spell cpu_usage_pct as 0-100. The arm normalizes
    instead of reading 45 percent as 45x the machine."""
    _pin_load(monkeypatch)
    sample = _macmon_sample(cpu_usage_pct=45.0)
    monkeypatch.setattr(dl, "read_macmon", lambda **k: (sample, None))
    monkeypatch.setattr(dl, "read_memory_pressure", lambda **k: (0.84, None))
    reading = dl.read_lanes()
    assert reading.arm("whole-machine cpu").value["busy_fraction"] == 0.45


@pytest.mark.skipif(
    shutil.which("macmon") is None,
    reason="macmon not on PATH; the live smoke needs the real sensor",
)
def test_live_macmon_smoke_answers_on_a_healthy_machine(monkeypatch) -> None:
    """The plan's own acceptance: macmon present, healthy machine -> a lane
    number and a per-arm reading list, from the REAL sensor."""
    _pin_load(monkeypatch)
    result = runner.invoke(app, ["doctor", "lanes", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["lane_count"] is not None
    states = {a["name"]: a["state"] for a in payload["arms"]}
    assert states["whole-machine cpu"] == dl.MEASURED
    assert states["memory"] == dl.MEASURED
