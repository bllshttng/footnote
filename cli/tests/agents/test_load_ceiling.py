"""x-3f84 W3: the CPU dimension of the spawn gate (`agents.max_load_per_cpu`).

The measured emergency: load 309 on 12 CPUs while the RAM floor held ten
times its margin. The one machine guard read the one resource that was never
scarce; this is the dimension beside it. Same contract as the RAM floor:
refuse never queues, <= 0 disables, unreadable skips."""
from __future__ import annotations

import pytest

from fno.agents import spawn_gate


@pytest.fixture(autouse=True)
def _fixed_cpus(monkeypatch):
    monkeypatch.setattr(spawn_gate.os, "cpu_count", lambda: 12)
    # Preferred on 3.13+ (affinity-aware, mirrors the Rust gate); patch both
    # spellings so the fixture holds on either interpreter.
    if hasattr(spawn_gate.os, "process_cpu_count"):
        monkeypatch.setattr(spawn_gate.os, "process_cpu_count", lambda: 12)


def _load(load1: float):
    return lambda: (load1, 0.0, 0.0)


def test_disabled_ceiling_never_fires(monkeypatch):
    monkeypatch.setattr(spawn_gate.os, "getloadavg", _load(309.0))
    spawn_gate._check_load_ceiling(0)  # no raise
    spawn_gate._check_load_ceiling(-1)


def test_over_ceiling_refuses_with_numbers(monkeypatch, capsys):
    """The refusal names the factor, the cpu count, the ceiling, and --force."""
    monkeypatch.setattr(spawn_gate.os, "getloadavg", _load(309.0))
    with pytest.raises(spawn_gate.GateRefused) as ei:
        spawn_gate._check_load_ceiling(8.0)
    assert ei.value.code == spawn_gate.EXIT_LOAD_REFUSED == 79
    err = capsys.readouterr().err
    assert "309" in err and "12 cpus" in err and "96" in err
    assert "--force" in err


def test_under_ceiling_passes(monkeypatch):
    monkeypatch.setattr(spawn_gate.os, "getloadavg", _load(24.0))
    spawn_gate._check_load_ceiling(8.0)  # 24 <= 96 on 12 cpus: no raise


def test_unreadable_load_skips_fail_open(monkeypatch, capsys):
    def boom():
        raise OSError("no loadavg here")

    monkeypatch.setattr(spawn_gate.os, "getloadavg", boom)
    spawn_gate._check_load_ceiling(8.0)  # no raise
    assert "skipping the load check" in capsys.readouterr().err


def test_ceiling_scales_with_cpu_count(monkeypatch):
    """One factor ports across machines: the same load flips verdicts either
    side of the per-cpu line as the cpu count changes."""
    monkeypatch.setattr(spawn_gate.os, "getloadavg", _load(20.0))
    _set_cpus(monkeypatch, 8)
    with pytest.raises(spawn_gate.GateRefused):
        spawn_gate._check_load_ceiling(2.0)  # 20 > 2 x 8
    _set_cpus(monkeypatch, 16)
    spawn_gate._check_load_ceiling(2.0)  # 20 <= 2 x 16: passes


def _set_cpus(monkeypatch, n):
    monkeypatch.setattr(spawn_gate.os, "cpu_count", lambda: n)
    if hasattr(spawn_gate.os, "process_cpu_count"):
        monkeypatch.setattr(spawn_gate.os, "process_cpu_count", lambda: n)


def test_config_default_and_coercion():
    from fno.config import AgentsBlock

    a = AgentsBlock()
    assert a.max_load_per_cpu == 8.0
    assert AgentsBlock(max_load_per_cpu=0).max_load_per_cpu == 0.0  # valid: off
    assert AgentsBlock(max_load_per_cpu="2.5").max_load_per_cpu == 2.5
    assert AgentsBlock(max_load_per_cpu="junk").max_load_per_cpu == 8.0
    assert AgentsBlock(max_load_per_cpu=True).max_load_per_cpu == 8.0
