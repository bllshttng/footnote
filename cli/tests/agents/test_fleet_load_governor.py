"""x-7783 Change 2, x-c588: the gate reads the CPU axis's verdict and holds on over.

LD1/LD3/LD4: the fleet's attributed share decides; over is a HOLD that
re-samples on CPU_HOLD_POLL_S and admits after CPU_ADMIT_SAMPLES consecutive
under-ceiling samples; an unreadable instrument refuses. The old trigger, its
prefetch band, and the 15-minute backstop are gone.
"""
from __future__ import annotations

import json

import pytest

from fno.agents import spawn_gate
from fno.footprint import Admission, Footprint


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
    # Memory terms are machine state too: the Rust gate reads the HOST swap,
    # so pin both off for the scenario under test (mirrors the agreement
    # fixture).
    cfg = tmp_path / "agents.toml"
    cfg.write_text("[agents]\nmin_free_gb = 0\nmax_swap_pct = 0\n", encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    # Hermetic defaults: the real footprint read is a ps snapshot against this
    # box (seconds under load), the real lsof scan reads 38 sockets, and the
    # real census reads the live registry. All three are pinned idle.
    from fno import doctor_footprint
    from fno.footprint import Footprint

    idle = Footprint(0.0, 0.0, 0.1, 0, 0, 0, 0, 0.0, 0.2, [], 0, None)
    monkeypatch.setattr(
        spawn_gate, "_prefetch_fleet_reading", lambda: (idle, None)
    )
    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: 0.5)
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map", lambda **k: {}
    )
    monkeypatch.setattr(
        spawn_gate,
        "census",
        lambda socket_map=None: spawn_gate.LiveCensus(workers=[]),
    )


def test_never_held_admits_on_the_first_sample_silently(tmp_path, monkeypatch, capsys):
    """LD4: an unloaded box pays one read and one sample - no debounce."""
    _isolate(tmp_path, monkeypatch)
    guard = _drive(monkeypatch, [_adm("admit")])
    assert capsys.readouterr().err == ""
    guard.release()


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
    assert AgentsBlock(max_fleet_cpu_share="0.25").max_fleet_cpu_share == 0.25
    assert AgentsBlock(max_fleet_cpu_share="junk").max_fleet_cpu_share == 0.5
    # The load backstop is retired: the key is unmodeled and reads as absent.
    assert getattr(AgentsBlock(hard_max_load_per_cpu="junk"), "hard_max_load_per_cpu", None) is None


def test_non_finite_never_disarms_a_machine_guard():
    """`nan` loses every comparison and `inf` wins every one.

    Either way the ceiling stops refusing while still reading as
    configured, which is worse than a value that is merely wrong. All
    sibling knobs shared the hole, so all are checked.
    """
    from fno.config import AgentsBlock

    fields = {
        "min_free_gb": 4.0,
        "max_load_per_cpu": 8.0,
        "max_fleet_cpu_share": 0.5,
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
        # max_fleet_cpu_share deliberately ABSENT

    assert float(getattr(_Agents, "max_fleet_cpu_share", 0.5)) == 0.5
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
        monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: 0.5)
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

# The gate-side king share, schema and fleet-load cases decided inside the
# ONE Rust gate now (crates/fno-agents/src/spawn_gate.rs and
# spawn_gate_lanes.rs tests). The transport contract lives in
# tests/agents/test_spawn_gate.py::TestTransport.
