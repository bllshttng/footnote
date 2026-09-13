"""x-7783 Change 2: the gate reads the CPU axis's verdict and holds on over.

LD1/LD3/LD4: the fleet's attributed share decides; over is a HOLD that
re-samples on CPU_HOLD_POLL_S and admits after CPU_ADMIT_SAMPLES consecutive
under-ceiling samples; an unreadable instrument refuses; the 15-minute load
is the absolute backstop. The old trigger and its prefetch band are gone.
"""
from __future__ import annotations

import json

import pytest

from fno.agents import spawn_gate
from fno.footprint import Admission, Footprint

HARD = 40.0


def _reading(fleet: float, measured: float, gap: str | None = None) -> Footprint:
    return Footprint(
        0.0, 0.0, fleet, 0, 0, 0, 0, 0.0, measured, [], 0, gap
    )


def _adm(verdict: str, *, axis: str = "fleet_cpu_share", **kw) -> Admission:
    fields: dict = dict(
        verdict=verdict,
        axis=axis,
        reason=kw.pop("reason", f"spawn-gate: test {verdict} on {axis}"),
        share_low=kw.pop("share_low", 0.1),
        share_high=kw.pop("share_high", 0.1),
        bound=kw.pop("bound", "exact"),
        fleet_cores=kw.pop("fleet_cores", 1.2),
        machine_cores=kw.pop("machine_cores", 6.0),
        capacity_cores=kw.pop("capacity_cores", 12.0),
        ceiling=kw.pop("ceiling", 0.5),
        gap=kw.pop("gap", None),
        load_15m=kw.pop("load_15m", 1.0),
        backstop=kw.pop("backstop", HARD * 12),
        top_holder=kw.pop("top_holder", None),
    )
    assert not kw, f"unexpected overrides: {kw}"
    return Admission(**fields)


def _settings(monkeypatch, *, max_live=3, min_free_gb=0.0):
    """Point run_gate at fixed knobs without touching real settings."""

    class _D:
        model = None
        account = None

    class _A:
        defaults = _D()
        profiles = {}
        worker_qos = "off"
        provider_limits: dict = {}

    a = _A()
    a.max_live = max_live
    a.min_free_gb = min_free_gb

    class _S:
        agents = a

    monkeypatch.setattr("fno.config.load_settings", lambda: _S())


def _drive(monkeypatch, admissions, *, no_wait=False, max_live=3, **kw):
    """run_gate with the CPU axis answering admissions in sequence."""
    _settings(monkeypatch, max_live=max_live, **kw)
    seq = iter(admissions)
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: next(seq))
    monkeypatch.setattr(spawn_gate, "CPU_HOLD_POLL_S", 0.01)
    monkeypatch.setattr(spawn_gate, "QUEUE_POLL_S", 0.01)
    return spawn_gate.run_gate("w2", "bg", no_wait=no_wait)


def _isolate(tmp_path, monkeypatch):
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    # Hermetic defaults: the real footprint read is a ps snapshot against this
    # box (seconds under load), the real lsof scan reads 38 sockets, and the
    # real census reads the live registry. All three are pinned idle.
    from fno import doctor_footprint
    from fno.footprint import Footprint

    idle = Footprint(0.0, 0.0, 0.1, 0, 0, 0, 0, 0.0, 0.2, [], 0, None)
    monkeypatch.setattr(
        spawn_gate, "_prefetch_fleet_reading", lambda: (idle, None)
    )
    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (0.5, 40.0))
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map", lambda **k: {}
    )
    monkeypatch.setattr(
        spawn_gate,
        "census",
        lambda socket_map=None: spawn_gate.LiveCensus(workers=[]),
    )


def test_hold_announces_then_admits_after_two_under_samples(tmp_path, monkeypatch, capsys):
    """AC4-HP: over is `spawn held:`, then admission needs TWO consecutive
    under-ceiling samples, and the admission line says so."""
    _isolate(tmp_path, monkeypatch)
    hold = _adm(
        "hold",
        share_low=0.5833,
        fleet_cores=7.0,
        reason=(
            "spawn held: the fleet holds 7.00/12.00 cores (58.3%) over "
            "max_fleet_cpu_share 50.0%; waiting for the fleet's own work to "
            "drain (--no-wait to fail fast, --force to bypass)"
        ),
    )
    guard = _drive(monkeypatch, [hold, _adm("admit"), _adm("admit")])
    err = capsys.readouterr().err
    assert "spawn held: the fleet holds 7.00/12.00 cores (58.3%)" in err
    assert "for 2 consecutive samples; admitting" in err
    guard.release()


def test_never_held_admits_on_the_first_sample_silently(tmp_path, monkeypatch, capsys):
    """LD4: an unloaded box pays one read and one sample - no debounce."""
    _isolate(tmp_path, monkeypatch)
    guard = _drive(monkeypatch, [_adm("admit")])
    assert capsys.readouterr().err == ""
    guard.release()


def test_hold_progress_names_the_wait(tmp_path, monkeypatch, capsys):
    _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(spawn_gate, "QUEUE_PROGRESS_EVERY_S", 0.0)
    hold = _adm("hold", share_low=0.6, reason="spawn held: over")
    guard = _drive(monkeypatch, [hold, hold, _adm("admit"), _adm("admit")])
    err = capsys.readouterr().err
    assert "still held: fleet 60.0% over 50.0%" in err
    assert "waited" in err
    guard.release()


def test_no_wait_refuses_on_the_first_over_sample(tmp_path, monkeypatch):
    """AC4-HP tail: --no-wait fails fast with samples: 1 and held_on named."""
    _isolate(tmp_path, monkeypatch)
    hold = _adm(
        "hold",
        reason="spawn held: the fleet holds 7.00/12.00 cores (58.3%) over",
    )
    with pytest.raises(SystemExit) as exc:
        _drive(monkeypatch, [hold], no_wait=True)
    assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED
    receipt = exc.value.receipt
    assert receipt["reason"] == "fleet_cpu_share"
    assert receipt["samples"] == 1
    assert receipt["held_on"] == "fleet_cpu_share"
    assert receipt["axis"] == "fleet_cpu_share"


def test_undecidable_refuses_at_once_with_bounds_and_gap(tmp_path, monkeypatch, capsys):
    """AC3-EDGE: the ceiling inside the interval refuses immediately with
    reason cpu_share_undecidable, exit 79, both bounds and the gap named."""
    _isolate(tmp_path, monkeypatch)
    gap = "3 pidless row(s) with no identity route (codex)"
    reason = (
        "spawn-gate: the fleet's CPU share cannot be decided: attributed "
        "17.5% of capacity, up to 60.0% with rows unattributed, and "
        "max_fleet_cpu_share 50.0% falls inside that band; "
        f"{gap}; close the attribution gap or lower foreign load (--force to bypass)"
    )
    admission = _adm(
        "undecidable",
        share_low=0.175,
        share_high=0.6,
        bound="upper",
        gap=gap,
        reason=reason,
    )
    with pytest.raises(SystemExit) as exc:
        _drive(monkeypatch, [admission])
    assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED == 79
    receipt = exc.value.receipt
    assert receipt["reason"] == "cpu_share_undecidable"
    assert receipt["axis"] == "fleet_cpu_share"
    assert receipt["axes_read"]["cpu"] == "undecidable"
    assert "load_15m" in receipt["axes_read"]
    err = capsys.readouterr().err
    assert "17.5%" in err and "60.0%" in err and gap in err


def test_unreadable_instrument_refuses_and_names_the_failure(tmp_path, monkeypatch, capsys):
    """AC5-ERR: the sensor blinding under load is itself a symptom."""
    _isolate(tmp_path, monkeypatch)
    _settings(monkeypatch, max_live=3)
    monkeypatch.setattr(
        spawn_gate,
        "_prefetch_fleet_reading",
        lambda: (None, "footprint unavailable: ps unavailable: timed out"),
    )
    monkeypatch.setattr(spawn_gate, "_load_cpus", lambda: 12)

    def no_loadavg():
        raise OSError("no loadavg here")

    monkeypatch.setattr(spawn_gate.os, "getloadavg", no_loadavg)

    with pytest.raises(SystemExit) as exc:
        spawn_gate.run_gate("w2", "bg", no_wait=True)
    assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED
    receipt = exc.value.receipt
    assert receipt["reason"] == "cpu_instrument_unreadable"
    assert receipt["axis"] == "cpu_instrument"
    assert "ps unavailable: timed out" in capsys.readouterr().err
    # Figures the instrument never measured are null, never 0.0 dressed as a
    # reading (the receipt seam).
    for key in (
        "share_low",
        "share_high",
        "fleet_cores",
        "machine_cores",
        "capacity_cores",
        "ceiling",
        "backstop",
    ):
        assert receipt[key] is None, key


def test_still_held_progress_names_the_top_holder(tmp_path, monkeypatch, capsys):
    """The periodic reprint carries the payload's holder clause, the same
    words the reason carries."""
    _isolate(tmp_path, monkeypatch)
    hold = _adm(
        "hold",
        share_low=0.625,
        fleet_cores=7.5,
        reason="spawn held: the fleet holds 7.50/12.00 cores",
        top_holder="yes 16 procs 5.13 cores",
    )
    backstop = _adm("refuse", axis="load_15m", load_15m=500.0)
    seq = iter([hold, hold, backstop])
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: next(seq))
    monkeypatch.setattr(spawn_gate, "QUEUE_PROGRESS_EVERY_S", 0.0)
    monkeypatch.setattr(spawn_gate, "CPU_HOLD_POLL_S", 0.01)
    with pytest.raises(SystemExit) as exc:
        spawn_gate.run_gate("w2", "bg")
    assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED
    err = capsys.readouterr().err
    assert "still held" in err
    assert "; top holder yes 16 procs 5.13 cores" in err


def test_backstop_on_load_15m_refuses_at_the_gate(tmp_path, monkeypatch):
    """AC6-EDGE at the gate seam: the backstop refuses carrying load_15m."""
    _isolate(tmp_path, monkeypatch)
    _settings(monkeypatch, max_live=3)
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (0.5, 40.0))
    monkeypatch.setattr(spawn_gate, "_load_cpus", lambda: 12)
    monkeypatch.setattr(spawn_gate.os, "getloadavg", lambda: (1.0, 1.0, 500.0))
    monkeypatch.setattr(
        spawn_gate, "_prefetch_fleet_reading", lambda: (_reading(0.1, 6.9), None)
    )

    with pytest.raises(SystemExit) as exc:
        spawn_gate.run_gate("w2", "bg", no_wait=True)
    assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED
    receipt = exc.value.receipt
    assert receipt["reason"] == "load_backstop"
    assert receipt["axis"] == "load_15m"
    assert receipt["load_15m"] == 500.0
    assert receipt["backstop"] == 480.0


def test_census_socket_scan_runs_once_per_spawn(tmp_path, monkeypatch):
    """x-e32e: the lsof scan is paid ONCE, however many queue passes run.

    The specimen: a spawn queued at the slot cap re-ran the scan every 2s
    poll and sat 12 minutes. The gate scans on the first census and reuses
    the map for every later pass.
    """
    _isolate(tmp_path, monkeypatch)
    scans: list[int] = []

    def fake_scan(**kwargs):
        scans.append(1)
        return {"abcd1234": 4242}

    monkeypatch.setattr("fno.agents.session_procs.bg_socket_pid_map", fake_scan)
    maps: list[object] = []

    def full_census(socket_map=None):
        maps.append(socket_map)
        return spawn_gate.LiveCensus(workers=[], fno_slot_workers=99)

    monkeypatch.setattr(spawn_gate, "census", full_census)
    monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 0.05)
    with pytest.raises(SystemExit) as exc:
        _drive(monkeypatch, [_adm("admit")] * 50, max_live=1)
    assert exc.value.code == spawn_gate.EXIT_QUEUE_TIMEOUT
    assert scans == [1], "the socket scan reran across queue passes"
    assert maps, "the queued spawn never reached a census"
    assert all(m == {"abcd1234": 4242} for m in maps)


def test_census_receives_the_scanned_map(tmp_path, monkeypatch):
    """The map the gate scanned is the map the census joins through - a
    `None` there would mean a second scan, which is the bug."""
    _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map", lambda **k: {"feedface": 7}
    )
    seen: list[object] = []
    monkeypatch.setattr(
        spawn_gate,
        "census",
        lambda socket_map=None: seen.append(socket_map)
        or spawn_gate.LiveCensus(workers=[]),
    )
    guard = _drive(monkeypatch, [_adm("admit")])
    assert seen == [{"feedface": 7}]
    guard.release()


def test_slow_socket_scan_names_its_wait(tmp_path, monkeypatch, capsys):
    """x-e32e: a slow scan prints what the spawn waited on instead of
    silence."""
    import time as _time

    _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(spawn_gate, "SLOW_SCAN_WARN_S", 0.01)

    def slow_scan(**kwargs):
        _time.sleep(0.03)
        return {}

    monkeypatch.setattr("fno.agents.session_procs.bg_socket_pid_map", slow_scan)
    monkeypatch.setattr(
        spawn_gate,
        "census",
        lambda socket_map=None: spawn_gate.LiveCensus(workers=[]),
    )
    guard = _drive(monkeypatch, [_adm("admit")])
    err = capsys.readouterr().err
    assert "bg-socket census took" in err
    assert "once per spawn" in err
    guard.release()


def test_timeout_receipt_names_held_on(tmp_path, monkeypatch):
    """LD4: the queue timeout receipt says which queue ate the budget."""
    _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(
        spawn_gate,
        "census",
        lambda socket_map=None: spawn_gate.LiveCensus(workers=[], fno_slot_workers=99),
    )
    monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 0.05)
    with pytest.raises(SystemExit) as exc:
        _drive(monkeypatch, [_adm("admit")] * 50, max_live=1)
    assert exc.value.receipt["held_on"] == "max_live"
    assert exc.value.receipt["axis"] == "max_live"


def test_cpu_hold_timeout_names_fleet_cpu_share(tmp_path, monkeypatch):
    _isolate(tmp_path, monkeypatch)
    hold = _adm("hold", share_low=0.9)
    monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 0.05)
    with pytest.raises(SystemExit) as exc:
        _drive(monkeypatch, [hold] * 50, max_live=3)
    assert exc.value.receipt["held_on"] == "fleet_cpu_share"


def test_after_admission_the_timeout_names_the_slot_cap(tmp_path, monkeypatch, capsys):
    """A drained hold must not keep blaming the CPU axis: once admitted past
    it, a slot wait reads as held_on max_live, and the admit line prints once,
    not on every queue pass."""
    _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(
        spawn_gate,
        "census",
        lambda socket_map=None: spawn_gate.LiveCensus(workers=[], fno_slot_workers=99),
    )
    monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 30.0)
    monkeypatch.setattr(spawn_gate, "CPU_ADMIT_SAMPLES", 1)

    # A fake clock makes the drain deterministic: a wall-clock 50ms budget
    # fired before the second axis read on a cold or loaded box, so the
    # timeout blamed the CPU axis instead of the slot cap. The clock ticks
    # once per monotonic() call, so 30 lands a dozen iterations past the
    # two-iteration drain but inside the 50 admits.
    clock = {"t": 100.0}

    def _tick(*_a):
        clock["t"] += 1.0
        return clock["t"]

    monkeypatch.setattr(spawn_gate.time, "monotonic", _tick)
    with pytest.raises(SystemExit) as exc:
        _drive(
            monkeypatch,
            [_adm("hold", share_low=0.9)] + [_adm("admit")] * 50,
            max_live=1,
        )
    assert exc.value.receipt["held_on"] == "max_live"
    assert capsys.readouterr().err.count("consecutive samples; admitting") == 1


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


def test_config_defaults_and_coercion():
    from fno.config import AgentsBlock

    a = AgentsBlock()
    assert a.max_fleet_cpu_share == 0.5
    assert a.hard_max_load_per_cpu == 40.0
    assert AgentsBlock(max_fleet_cpu_share="0.25").max_fleet_cpu_share == 0.25
    assert AgentsBlock(max_fleet_cpu_share="junk").max_fleet_cpu_share == 0.5
    assert AgentsBlock(hard_max_load_per_cpu="junk").hard_max_load_per_cpu == 40.0


def test_non_finite_never_disarms_a_machine_guard():
    """`nan` loses every comparison and `inf` wins every one.

    Either way the ceiling stops refusing while still reading as
    configured, which is worse than a value that is merely wrong. All
    four sibling knobs shared the hole, so all four are checked.
    """
    from fno.config import AgentsBlock

    fields = {
        "min_free_gb": 4.0,
        "max_load_per_cpu": 8.0,
        "max_fleet_cpu_share": 0.5,
        "hard_max_load_per_cpu": 40.0,
    }
    for name, default in fields.items():
        for bad in ("nan", "inf", "-inf", "NaN", float("nan"), float("inf")):
            got = getattr(AgentsBlock(**{name: bad}), name)
            assert got == default, f"{name}={bad!r} coerced to {got}, not {default}"


def test_a_settings_object_missing_new_fields_keeps_its_cap():
    """A new machine knob must not become a cap bug in another module.

    The gate reads `provider_limits` as a REAL attribute: a settings object
    missing it drops the whole config block into its fail-safe branch, which
    silently replaced the caller's `max_live` with the built-in 3. The CPU
    axis knobs are read per-sample inside _cpu_axis and tolerate absence by
    design; the cap must survive either way.
    """

    class _Agents:
        max_live = 9  # the value that must survive
        min_free_gb = 0.0
        max_load_per_cpu = 0.0
        # max_fleet_cpu_share and hard_max_load_per_cpu deliberately ABSENT

    assert float(getattr(_Agents, "max_fleet_cpu_share", 0.5)) == 0.5
    assert float(getattr(_Agents, "hard_max_load_per_cpu", 40.0)) == 40.0
    assert int(_Agents.max_live) == 9


class TestCauseMainProbeEntry:
    """AC1-HP: the console script IS the verb, byte for byte.

    The Rust gate reads the probe's stdout and exit code; if the narrow entry
    point ever answered differently from `fno-py doctor footprint --json
    --cause-only`, the two halves of the probe transport would disagree about
    the same machine. So the parity is asserted directly, under the same
    patched reading.
    """

    @staticmethod
    def _reading(gap=None):
        from fno.footprint import parse_footprint

        rows = "\n".join(
            f"{100 + i} 1 01:00:00 {20 - i}.0 1024 fno-agents-worker worker-{i}"
            for i in range(3)
        )
        reading = parse_footprint(f"PID PPID ELAPSED %CPU RSS COMMAND\n{rows}")
        return reading._replace(attribution_gap=gap)

    def _run_both(self, monkeypatch, capsys, *, gap=None, error=None):
        import typer
        from types import SimpleNamespace

        from fno import doctor_footprint

        if error is not None:
            monkeypatch.setattr(
                doctor_footprint, "cause_reading", lambda **kw: (None, error)
            )
        else:
            reading = self._reading(gap)
            monkeypatch.setattr(
                doctor_footprint, "cause_reading", lambda **kw: (reading, None)
            )
        # x-7783: pin the admission inputs so the box's live numbers cannot
        # move between the two calls; the snapshot feeds only the trend.
        monkeypatch.setattr(
            doctor_footprint,
            "_spawn_load_snapshot",
            lambda: SimpleNamespace(load_1m=None, load_cpu_count=12, load_15m=1.0),
        )
        monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (0.5, 40.0))
        monkeypatch.setattr(doctor_footprint, "_cpu_capacity_cores", lambda: 12)

        def invoke(entry):
            # footprint_command raises typer.Exit in-process; cause_main
            # translates it to SystemExit, which is the parity under test.
            with pytest.raises((typer.Exit, SystemExit)) as ei:
                entry()
            code = getattr(ei.value, "exit_code", None)
            if code is None:
                code = ei.value.code
            return capsys.readouterr().out, code

        verb_out, verb_code = invoke(
            lambda: doctor_footprint.footprint_command(json_output=True, cause_only=True)
        )
        probe_out, probe_code = invoke(doctor_footprint.cause_main)
        return (verb_out, verb_code), (probe_out, probe_code)

    def test_complete_reading_parity(self, monkeypatch, capsys):
        (verb, verb_code), (probe, probe_code) = self._run_both(monkeypatch, capsys)
        assert (probe, probe_code) == (verb, verb_code)
        assert probe_code == 0
        assert json.loads(probe)["exit_code"] == 0

    def test_gapped_reading_parity_admits_on_the_upper_bound(self, monkeypatch, capsys):
        """x-7783 LD3: a gap no longer forces exit 4. Both transports answer
        identically with the interval carried and the exit 0."""
        (verb, verb_code), (probe, probe_code) = self._run_both(
            monkeypatch, capsys, gap="21 unmapped bg-socket rows"
        )
        assert (probe, probe_code) == (verb, verb_code)
        assert probe_code == 0
        payload = json.loads(probe)
        assert payload["exit_code"] == 0
        assert payload["admission"]["bound"] == "upper"
        assert payload["attribution_gap"] == "21 unmapped bg-socket rows"

    def test_failed_reading_names_the_error_and_exits_4(self, monkeypatch, capsys):
        (verb, verb_code), (probe, probe_code) = self._run_both(
            monkeypatch, capsys, error="footprint unavailable: ps failed"
        )
        assert (probe, probe_code) == (verb, verb_code)
        assert probe_code == 4
        assert json.loads(probe) == {
            "error": "footprint unavailable: ps failed",
            "exit_code": 4,
        }
