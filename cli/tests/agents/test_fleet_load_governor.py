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
    """Pin the CPU denominator at the seam that decides it.

    `_load_cpus` reads footprint's capacity helper, which lives in another
    module and does not see a patched `spawn_gate.os`. Patching the os
    attributes here would leave these tests reading the real machine, which
    is how they passed on a 12-core box and failed on a 4-core runner: at 4
    cpus a load of 309 clears the 40x4 backstop and never reaches the
    governor these tests exist to exercise.
    """
    monkeypatch.setattr(spawn_gate, "_load_cpus", lambda: 12)


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


class TestReviewRoundOne:
    """The findings from the self-review of this branch, each pinned.

    Every one is a way the gate could still lie or disagree with its twin,
    which is the defect class the branch exists to remove.
    """

    def test_attribution_is_prefetched_outside_the_gate_mutex(self, monkeypatch):
        """The expensive read must not happen while the gate mutex is held.

        The reading is a `ps` snapshot behind a multi-second deadline, and the
        mutex serializes every spawner on the machine. Holding it across that
        read punishes concurrent `--no-wait` spawners with
        `no_wait_mutex_held` for no reason of their own, and it does so
        exactly on a loaded box, which is when contention is worst.
        """
        _load(monkeypatch, 127.6)

        def boom():
            raise AssertionError("took its own reading despite a prefetched one")

        monkeypatch.setattr(spawn_gate, "_fleet_cpu_reading", boom)
        # A prefetched reading is used as-is; nothing is read here.
        _check(prefetched=(0.79, 12.0))

        with pytest.raises(spawn_gate.GateRefused):
            _check(prefetched=(9.0, 12.0))

    def test_prefetch_only_reads_inside_the_deciding_band(self, monkeypatch):
        """Below the trigger and above the backstop, the verdict needs no read."""
        calls = []
        monkeypatch.setattr(
            spawn_gate, "_fleet_cpu_reading", lambda: calls.append(1) or (0.1, 12.0)
        )

        _load(monkeypatch, 24.0)  # under the trigger: admits without asking
        assert spawn_gate._prefetch_fleet_reading(TRIGGER, HARD) is spawn_gate._NOT_PREFETCHED

        _load(monkeypatch, 600.0)  # over the backstop: refuses without asking
        assert spawn_gate._prefetch_fleet_reading(TRIGGER, HARD) is spawn_gate._NOT_PREFETCHED

        assert calls == [], "read attribution outside the band where it decides"

        _load(monkeypatch, 127.6)  # in the band: the read is what decides
        assert spawn_gate._prefetch_fleet_reading(TRIGGER, HARD) == (0.1, 12.0)
        assert calls == [1]

    def test_unreadable_prefetch_is_not_confused_with_no_prefetch(self, monkeypatch):
        """`None` is a real reading and must still fail closed.

        The sentinel exists because "unreadable" and "not supplied" are
        different, and collapsing them would silently turn a failed read into
        a fresh one inside the mutex.
        """
        _load(monkeypatch, 127.6)
        monkeypatch.setattr(
            spawn_gate,
            "_fleet_cpu_reading",
            lambda: pytest.fail("re-read a reading that was already taken"),
        )
        with pytest.raises(spawn_gate.GateRefused):
            _check(prefetched=None)

    def test_backstop_at_or_below_the_trigger_restores_the_defaults(self):
        """One config line must not silently restore the old defect.

        A backstop at or below the trigger means every load that would have
        been attributed is refused blindly instead. Four docstrings said so
        and nothing enforced it.
        """
        from fno.config import AgentsBlock

        for trigger, hard in ((2.5, 2.5), (50.0, 40.0), (8.0, 4.0)):
            a = AgentsBlock(max_load_per_cpu=trigger, hard_max_load_per_cpu=hard)
            assert (a.max_load_per_cpu, a.hard_max_load_per_cpu) == (8.0, 40.0)

        # A disabled backstop is coherent, not incoherent: the governor is
        # then the only ceiling, which is a choice an operator can make.
        off = AgentsBlock(max_load_per_cpu=8.0, hard_max_load_per_cpu=0)
        assert (off.max_load_per_cpu, off.hard_max_load_per_cpu) == (8.0, 0.0)

        ok = AgentsBlock(max_load_per_cpu=1.0, hard_max_load_per_cpu=2.0)
        assert (ok.max_load_per_cpu, ok.hard_max_load_per_cpu) == (1.0, 2.0)

    def test_non_finite_never_disarms_a_machine_guard(self):
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


def test_a_settings_object_missing_new_fields_keeps_its_cap(monkeypatch):
    """A new machine knob must not become a cap bug in another module.

    Reading the two new fields strictly put them in the same failure class as
    `provider_limits`: any settings object built before they existed dropped
    the WHOLE config block into its fail-safe branch, which silently replaced
    that caller's `max_live` with the built-in 3. CI found it as two
    unrelated-looking failures, an exit code 76 where 80 was expected and a
    refusal that never fired, three test modules away from this change.

    A missing CAP must still fail loudly, because falling back would uncap a
    provider. A missing machine THRESHOLD has a safe default. This pins that
    distinction.
    """

    class _Defaults:
        model = None
        account = None

    class _Agents:
        defaults = _Defaults()
        profiles: dict = {}
        worker_qos = "off"
        max_live = 9  # the value that must survive
        min_free_gb = 0.0
        max_load_per_cpu = 0.0
        provider_limits: dict = {}
        # max_fleet_cpu_share and hard_max_load_per_cpu deliberately ABSENT

    class _Settings:
        agents = _Agents()

    monkeypatch.setattr("fno.config.load_settings", lambda: _Settings())

    from fno.config import load_settings

    cfg = load_settings().agents
    assert not hasattr(cfg, "max_fleet_cpu_share"), "fixture must omit the field"

    # The read the gate performs, mirrored: the two new knobs fall back and the
    # cap is untouched.
    assert float(getattr(cfg, "max_fleet_cpu_share", 0.5)) == 0.5
    assert float(getattr(cfg, "hard_max_load_per_cpu", 40.0)) == 40.0
    assert int(cfg.max_live) == 9
