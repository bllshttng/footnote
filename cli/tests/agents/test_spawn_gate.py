"""Spawn gate (x-c5cc): union live-count, RAM floor, queue loop, QoS wrap.

FNO_THINK_SPAWN=0 discipline is irrelevant here (nothing dispatches), but
every test redirects FNO_CLAUDE_DAEMON_DIR + FNO_CLAIMS_ROOT so no real
roster or claims dir is touched.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from fno.agents import spawn_gate
from fno.agents.registry import AgentEntry


@pytest.fixture(autouse=True)
def _isolated_world(tmp_path, monkeypatch):
    """No test reads the real roster, claims root, or settings."""
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    monkeypatch.setenv("FNO_THINK_SPAWN", "0")
    # conftest disables the gate suite-wide; re-arm it here — these tests
    # exercise the gate itself.
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    yield


def _write_roster(tmp_path, workers: dict) -> None:
    roster = {"proto": 1, "supervisorPid": 1, "workers": workers}
    (tmp_path / "daemon" / "roster.json").write_text(json.dumps(roster))


def _row(name: str, *, status="live", pid=None, short_id=""):
    return AgentEntry(
        name=name,
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/log",
        status=status,
        pid=pid,
        short_id=short_id,
    )


ALIVE = os.getpid()  # a pid that is definitely alive (this test process)


class TestCensus:
    def test_pid_start_token_mismatch_is_not_our_process(self, monkeypatch):
        """A reused numeric PID must not keep an old registry row alive."""
        class Proc:
            def is_running(self):
                return True

            def status(self):
                return "running"

        class Psutil:
            STATUS_ZOMBIE = "zombie"

            @staticmethod
            def Process(_pid):
                return Proc()

        monkeypatch.setattr(
            spawn_gate, "_process_start_time",
            lambda _pid, _psutil=None: 42_000_000,
            raising=False,
        )
        assert not spawn_gate._pid_alive(4242, 41_000_000, _psutil=Psutil)
        assert spawn_gate._pid_alive(4242, 42_000_000, _psutil=Psutil)

        monkeypatch.setattr(spawn_gate, "_process_start_time", lambda *_args: None)
        assert spawn_gate._pid_alive(4242, 42_000_000, _psutil=Psutil) is None
        assert not spawn_gate._pid_alive(4242, 42_000_000, _psutil=Psutil)

    def test_union_counts_and_dedups_adopted_session(self, tmp_path, monkeypatch):
        """AC1-EDGE: 1 fno pane worker + 1 foreign roster worker + 1 adopted
        session (roster row AND minted registry row) -> count 3."""
        _write_roster(
            tmp_path,
            {
                "aaaaaaaa": {"sessionId": "aaaaaaaa-1-2-3-4", "pid": ALIVE},
                "bbbbbbbb": {"sessionId": "bbbbbbbb-1-2-3-4", "pid": ALIVE},
            },
        )
        rows = [
            _row("pane-worker", pid=ALIVE),  # fno-only
            _row("adopted", pid=ALIVE, short_id="bbbbbbbb"),  # dup of roster
        ]
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        c = spawn_gate.census()
        assert c.count == 3
        assert not c.warnings

    def test_dead_pids_contribute_zero(self, tmp_path, monkeypatch):
        """AC1-EDGE2 / AC4-EDGE: reaped processes free slots."""
        _write_roster(
            tmp_path,
            # pid 2**22+17 is (realistically) never alive; None pid = disk-only.
            {
                "cccccccc": {"sessionId": "cccccccc-1-2-3-4", "pid": 4194321},
                "dddddddd": {"sessionId": "dddddddd-1-2-3-4"},
            },
        )
        rows = [_row("dead-worker", status="live", pid=4194321)]
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        assert spawn_gate.census().count == 0

    def test_unknown_process_incarnation_counts_conservatively(self, monkeypatch):
        row = _row("unreadable-worker", pid=ALIVE)
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_args: None)

        result = spawn_gate.census()

        assert result.count == 1
        assert result.fno_slot_workers == 1
        assert [worker.name for worker in result.workers] == ["unreadable-worker"]
        assert any("incarnation unreadable" in warning for warning in result.warnings)

    def test_non_live_statuses_never_counted(self, monkeypatch):
        rows = [
            _row("gone", status="exited", pid=ALIVE),
            _row("dead", status="permanent_dead", pid=ALIVE),
            _row("orphan", status="orphaned", pid=ALIVE),
        ]
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        assert spawn_gate.census().count == 0

    def test_malformed_roster_fails_open_with_warning(self, tmp_path, monkeypatch):
        (tmp_path / "daemon" / "roster.json").write_text("{ not json")
        rows = [_row("ok", pid=ALIVE)]
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        c = spawn_gate.census()
        assert c.count == 1, "registry still counts when the roster is garbage"
        assert any("roster unreadable" in w for w in c.warnings)

    def test_missing_roster_is_silent_zero(self, monkeypatch):
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        c = spawn_gate.census()
        assert c.count == 0
        assert not c.warnings

    def test_broken_registry_fails_open_with_warning(self, tmp_path, monkeypatch):
        _write_roster(
            tmp_path, {"eeeeeeee": {"sessionId": "eeeeeeee-1-2-3-4", "pid": ALIVE}}
        )

        def boom():
            raise RuntimeError("registry exploded")

        monkeypatch.setattr("fno.agents.registry.load_registry", boom)
        c = spawn_gate.census()
        assert c.count == 1, "roster still counts when the registry is broken"
        assert any("registry unreadable" in w for w in c.warnings)

    def test_headless_slot_claims_count(self, monkeypatch):
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        from fno.claims.core import acquire_claim
        from fno.claims.io import global_claims_root

        acquire_claim(
            "worker:one-shot", "h1", ttl_ms=60_000, root=global_claims_root()
        )
        assert spawn_gate.census().count == 1

    def test_live_registry_row_deduplicates_worker_claim(self, monkeypatch):
        monkeypatch.setattr(
            "fno.agents.registry.load_registry",
            lambda: [_row("revived", pid=ALIVE)],
        )
        from fno.claims.core import acquire_claim
        from fno.claims.io import global_claims_root

        acquire_claim(
            "worker:revived", "h1", ttl_ms=60_000, root=global_claims_root()
        )

        result = spawn_gate.census()

        assert result.fno_slot_workers == 1
        assert result.slot_claims == 0
        assert result.slot_count == 1

    def test_subagent_source_is_outside_slot_arithmetic(self, tmp_path, monkeypatch):
        """AC6-INV (x-af92): the sidechain discovery source never feeds census().

        census() counts only live registry rows + headless slot claims;
        subagents live in the projects transcript store, which census does not
        read. This pins both halves: census() never calls the sidechain reader,
        and sidechain transcripts on disk do not move slot_count, so a
        display-only visibility feature can never alter spawn admission.
        """
        monkeypatch.setattr(
            "fno.agents.registry.load_registry",
            lambda: [_row("w1", status="busy", pid=ALIVE, short_id="aaaa0000")],
        )
        # Sidechain transcripts present in the projects store census never reads.
        sdir = tmp_path / "projects" / "-c" / "p-1-2-3-4-5" / "subagents"
        sdir.mkdir(parents=True)
        (sdir / "agent-dead0000000001.jsonl").write_text(
            json.dumps(
                {
                    "isSidechain": True,
                    "agentId": "dead0000000001",
                    "sessionId": "p-1-2-3-4-5",
                    "type": "user",
                }
            )
            + "\n"
        )
        monkeypatch.setenv("FNO_CLAUDE_PROJECTS_DIR", str(tmp_path / "projects"))

        # If a future change wires the sidechain reader into census(), this fires.
        def _fail_if_called(*a, **k):
            raise AssertionError(
                "census() must not call discover_subagents; the sidechain "
                "source is display-only (x-af92 AC6-INV)"
            )

        monkeypatch.setattr(
            "fno.agents.discover.discover_subagents", _fail_if_called
        )

        c = spawn_gate.census()
        assert c.slot_count == 1  # the one live registry row; nothing from sidechains
        assert not any("dead0000000001" in (w.name or "") for w in c.workers)


class TestRamFloor:
    def test_disabled_floor_never_fires(self, monkeypatch):
        monkeypatch.setattr(spawn_gate, "available_ram_gb", lambda: 0.001)
        spawn_gate._check_ram_floor(0)  # no raise
        spawn_gate._check_ram_floor(-1)

    def test_below_floor_refuses_with_numbers(self, monkeypatch, capsys):
        """AC2-ERR / AC2-UI: refusal names floor + measured value + --force."""
        monkeypatch.setattr(spawn_gate, "available_ram_gb", lambda: 1.5)
        with pytest.raises(SystemExit) as exc:
            spawn_gate._check_ram_floor(4.0)
        assert exc.value.code == spawn_gate.EXIT_RAM_REFUSED
        err = capsys.readouterr().err
        assert "1.5" in err and "4.0" in err and "--force" in err

    def test_exactly_at_floor_passes(self, monkeypatch):
        monkeypatch.setattr(spawn_gate, "available_ram_gb", lambda: 4.0)
        spawn_gate._check_ram_floor(4.0)

    def test_unreadable_ram_fails_open(self, monkeypatch, capsys):
        """AC2-EDGE: unparseable read skips the guard with one warning."""
        monkeypatch.setattr(spawn_gate, "available_ram_gb", lambda: None)
        spawn_gate._check_ram_floor(4.0)
        assert "skipping the floor check" in capsys.readouterr().err


def _settings(monkeypatch, *, max_live=3, min_free_gb=0.0, max_lanes=None, max_load_per_cpu=0.0):
    """Point run_gate at fixed knobs without touching real settings."""

    class _D:
        model = None
        account = None

    class _A:
        defaults = _D()
        profiles = {}
        worker_qos = "off"

    a = _A()
    a.max_live = max_live
    a.min_free_gb = min_free_gb
    a.max_load_per_cpu = max_load_per_cpu  # 0 = the CPU guard stays off in tests
    a.provider_limits = {"zai": 5} if max_lanes is None else max_lanes

    class _S:
        pass

    s = _S()
    s.agents = a
    monkeypatch.setattr("fno.config.load_settings", lambda: s)


class TestRunGate:
    def test_footprint_cause_reader_formats_fleet_share(self, monkeypatch):
        from fno import doctor_footprint

        monkeypatch.setattr(doctor_footprint, "_live_root_pids", lambda **_kwargs: set())
        monkeypatch.setattr(
            doctor_footprint,
            "_read_ps",
            lambda **_kwargs: (
                """\
                PID PPID ELAPSED %CPU RSS COMMAND
                100 1 01:00:00 86.0 1024 fno-agents-worker --run
                101 100 01:00:00 100.0 1024 cargo test -p fno
                200 1 01:00:00 134.0 1024 unrelated-build
                """,
                None,
            ),
        )
        monkeypatch.setattr(spawn_gate.os, "cpu_count", lambda: 12)
        monkeypatch.setattr(spawn_gate.os, "process_cpu_count", lambda: 12, raising=False)

        evidence = spawn_gate._footprint_cause_evidence()

        assert evidence is not None
        assert "footprint attributes 1.86/12.00 cores" in evidence
        assert "15.5% capacity" in evidence

    def test_footprint_cause_reader_fails_open_when_ps_is_unavailable(
        self, monkeypatch
    ):
        from fno import doctor_footprint

        monkeypatch.setattr(
            doctor_footprint, "_read_ps", lambda **_kwargs: (None, "ps unavailable")
        )

        assert spawn_gate._footprint_cause_evidence() is None

    def test_footprint_cause_reader_uses_bounded_ps_timeout(self, monkeypatch):
        from fno import doctor_footprint

        calls: list[dict[str, object]] = []

        def unavailable_ps(**kwargs):
            calls.append(kwargs)
            return None, "ps unavailable: timed out after 5.0s"

        monkeypatch.setattr(doctor_footprint, "_read_ps", unavailable_ps)

        assert spawn_gate._footprint_cause_evidence() is None
        assert calls == [{"timeout": 5.0}]

    def test_under_cap_passes_silently(self, monkeypatch, capsys):
        """AC1-HP: nothing on stderr, no queue, guard holds the mutex."""
        _settings(monkeypatch, max_live=3)
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        guard = spawn_gate.run_gate("w2", "bg")
        assert capsys.readouterr().err == ""
        guard.release()

    def test_over_load_ceiling_refuses_under_cap(self, monkeypatch, capsys):
        """x-3f84 W3 wiring: the CPU guard fires beside the RAM floor, with
        slots FREE - a machine can be oversubscribed while under cap, which is
        the whole reason the dimension exists."""
        _settings(monkeypatch, max_live=3, max_load_per_cpu=8.0)
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        monkeypatch.setattr(spawn_gate.os, "cpu_count", lambda: 12)
        monkeypatch.setattr(spawn_gate.os, "getloadavg", lambda: (309.0, 0.0, 0.0))
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg", no_wait=True)
        assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED

    def test_over_load_reports_fleet_cause_evidence(self, monkeypatch, capsys):
        _settings(monkeypatch, max_live=3, max_load_per_cpu=8.0)
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        monkeypatch.setattr(spawn_gate.os, "cpu_count", lambda: 12)
        monkeypatch.setattr(spawn_gate.os, "getloadavg", lambda: (309.0, 0.0, 0.0))
        monkeypatch.setattr(
            spawn_gate,
            "_footprint_cause_evidence",
            lambda: "spawn-gate: footprint attributes 1.86/12.00 cores (15.5% capacity, 58.0% of measured CPU) to the fleet",
            raising=False,
        )

        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg", no_wait=True)

        assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED
        error = capsys.readouterr().err
        assert "1-min load 309.0 exceeds" in error
        assert "footprint attributes 1.86/12.00 cores" in error

    def test_over_load_keeps_refusal_when_fleet_cause_is_unavailable(
        self, monkeypatch, capsys
    ):
        _settings(monkeypatch, max_live=3, max_load_per_cpu=8.0)
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        monkeypatch.setattr(spawn_gate.os, "cpu_count", lambda: 12)
        monkeypatch.setattr(spawn_gate.os, "getloadavg", lambda: (309.0, 0.0, 0.0))
        monkeypatch.setattr(
            spawn_gate, "_footprint_cause_evidence", lambda: None, raising=False
        )

        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg", no_wait=True)

        assert exc.value.code == spawn_gate.EXIT_LOAD_REFUSED
        error = capsys.readouterr().err
        assert "footprint cause unavailable; load refusal unchanged" in error
        assert "1-min load 309.0 exceeds" in error

    def test_at_cap_no_wait_refuses(self, monkeypatch, capsys):
        _settings(monkeypatch, max_live=1)
        w = spawn_gate.LiveWorker("fno", "w1", "claude", "bg", ALIVE, "busy")
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[w], fno_slot_workers=1)
        )
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg", no_wait=True)
        assert exc.value.code == spawn_gate.EXIT_NO_WAIT
        assert "fno agents top" in capsys.readouterr().err

    def test_queue_announces_then_dispatches_when_slot_frees(
        self, monkeypatch, capsys
    ):
        """AC1-UI: the queue line prints BEFORE the first poll sleep."""
        _settings(monkeypatch, max_live=1)
        w = spawn_gate.LiveWorker("fno", "w1", "claude", "bg", ALIVE, "busy")
        calls = {"n": 0}

        def fake_census():
            calls["n"] += 1
            if calls["n"] == 1:
                return spawn_gate.LiveCensus(workers=[w], fno_slot_workers=1)
            return spawn_gate.LiveCensus(workers=[])

        monkeypatch.setattr(spawn_gate, "census", fake_census)
        monkeypatch.setattr(spawn_gate, "QUEUE_POLL_S", 0.01)
        guard = spawn_gate.run_gate("w2", "bg")
        err = capsys.readouterr().err
        assert "spawn queued: 1 live worker slots >= max_live 1" in err
        assert "--no-wait" in err and "--force" in err
        guard.release()

    def test_queue_timeout_is_distinct_and_loud(self, monkeypatch, capsys):
        """AC1-ERR: reusable gate returns refusal data without writing stdout."""
        _settings(monkeypatch, max_live=1)
        w = spawn_gate.LiveWorker("fno", "w1", "claude", "bg", ALIVE, "busy")
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[w], fno_slot_workers=1)
        )
        monkeypatch.setattr(spawn_gate, "QUEUE_POLL_S", 0.01)
        monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 0.05)
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg")
        assert exc.value.code == spawn_gate.EXIT_QUEUE_TIMEOUT
        captured = capsys.readouterr()
        assert "fno agents top" in captured.err
        assert exc.value.code not in (2, 13, 14, 15, 18, 127)
        assert captured.out == ""
        receipt = exc.value.receipt
        assert receipt["status"] == "refused"
        assert receipt["reason"] == "queue_timeout"
        assert receipt["max_live"] == 1
        assert receipt["count"] == 1
        assert receipt["current_count"] == 1

    def test_queue_timeout_spawn_command_emits_receipt_and_exits_75(self, monkeypatch):
        """fno agents spawn exits non-zero (75) on queue timeout and emits machine-readable receipt."""
        from typer.testing import CliRunner
        from fno.agents.cli import agents_app

        _settings(monkeypatch, max_live=2)
        w1 = spawn_gate.LiveWorker("fno", "w1", "claude", "bg", ALIVE, "busy")
        w2 = spawn_gate.LiveWorker("fno", "w2", "claude", "bg", ALIVE, "busy")
        monkeypatch.setattr(
            spawn_gate,
            "census",
            lambda: spawn_gate.LiveCensus(workers=[w1, w2], fno_slot_workers=2),
        )
        monkeypatch.setattr(spawn_gate, "QUEUE_POLL_S", 0.01)
        monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 0.05)

        runner = CliRunner()
        res = runner.invoke(
            agents_app,
            ["spawn", "do something", "--name", "w3", "-H", "claude", "-m", "claude-sonnet-4-6"],
        )
        assert res.exit_code == spawn_gate.EXIT_QUEUE_TIMEOUT
        assert "spawn-gate: queue timeout after 0s at max_live 2" in res.output
        receipt_line = [line for line in res.output.splitlines() if line.startswith("{")][-1]
        receipt = json.loads(receipt_line)
        assert receipt["status"] == "refused"
        assert receipt["reason"] == "queue_timeout"
        assert receipt["max_live"] == 2
        assert receipt["count"] == 2
        assert receipt["current_count"] == 2

    def test_force_bypasses_and_prints_forced_line(self, monkeypatch, capsys):
        _settings(monkeypatch, max_live=1)
        w = spawn_gate.LiveWorker("fno", "w1", "claude", "bg", ALIVE, "busy")
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[w], fno_slot_workers=1)
        )
        guard = spawn_gate.run_gate("w2", "bg", force=True)
        assert "forced past cap" in capsys.readouterr().err
        guard.release()

    def test_no_wait_refuses_fast_when_the_mutex_is_contended(
        self, monkeypatch, capsys
    ):
        """A busy gate mutex is queueing, so --no-wait must refuse on it.

        Regression: the no_wait check used to live INSIDE the acquired-mutex
        branch, so a spawner that died mid-gate (leaving the claim `suspect`
        for its full TTL) made every --no-wait caller wait the whole
        QUEUE_TIMEOUT_S and then exit EXIT_QUEUE_TIMEOUT - unable to tell
        "cap is full" from "the gate is wedged".
        """
        _settings(monkeypatch, max_live=99)  # cap is irrelevant: never counted
        monkeypatch.setattr(spawn_gate, "_acquire_gate_mutex", lambda _h: False)
        # Long enough that a fall-through to the queue loop is unmistakable.
        monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 30.0)
        monkeypatch.setattr(spawn_gate, "QUEUE_POLL_S", 0.01)
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg", no_wait=True)
        assert exc.value.code == spawn_gate.EXIT_NO_WAIT
        assert "gate mutex" in capsys.readouterr().err

    def test_permanently_held_mutex_proceeds_unserialized(
        self, monkeypatch, capsys
    ):
        """A wedged mutex must not brick spawning (the module's fail-open rule).

        Regression: with no bound on the mutex wait, one spawner that died
        inside the critical section queued EVERY other spawner on the machine
        behind its corpse until each hit its own 600s timeout.
        """
        _settings(monkeypatch, max_live=3)
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        monkeypatch.setattr(spawn_gate, "_acquire_gate_mutex", lambda _h: False)
        monkeypatch.setattr(spawn_gate, "MUTEX_WAIT_BUDGET_S", 0.05)
        monkeypatch.setattr(spawn_gate, "QUEUE_POLL_S", 0.01)
        monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 30.0)
        guard = spawn_gate.run_gate("w2", "bg")  # must return, not hang
        assert "proceeding unserialized" in capsys.readouterr().err
        guard.release()

    def test_mutex_budget_resets_on_a_successful_acquire(self, monkeypatch, capsys):
        """The budget tracks an UNBROKEN contention run, not total queue time.

        A long legitimate queue (mutex acquired every poll, cap simply full)
        must never accumulate into a spurious "proceeding unserialized".
        """
        _settings(monkeypatch, max_live=1)
        w = spawn_gate.LiveWorker("fno", "w1", "claude", "bg", ALIVE, "busy")
        monkeypatch.setattr(
            spawn_gate,
            "census",
            lambda: spawn_gate.LiveCensus(workers=[w], fno_slot_workers=1),
        )
        monkeypatch.setattr(spawn_gate, "_acquire_gate_mutex", lambda _h: True)
        monkeypatch.setattr(spawn_gate, "MUTEX_WAIT_BUDGET_S", 0.01)
        monkeypatch.setattr(spawn_gate, "QUEUE_POLL_S", 0.01)
        monkeypatch.setattr(spawn_gate, "QUEUE_TIMEOUT_S", 0.2)
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg")
        assert exc.value.code == spawn_gate.EXIT_QUEUE_TIMEOUT
        assert "proceeding unserialized" not in capsys.readouterr().err

    def test_dequeue_ram_recheck_refuses(self, monkeypatch):
        """AC2-FR: a freed slot still refuses when RAM dropped meanwhile."""
        _settings(monkeypatch, max_live=1, min_free_gb=4.0)
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        monkeypatch.setattr(spawn_gate, "available_ram_gb", lambda: 1.0)
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("w2", "bg")
        assert exc.value.code == spawn_gate.EXIT_RAM_REFUSED

    def test_headless_holds_worker_slot_claim(self, monkeypatch):
        """AC1-FR territory: a headless pass leaves a visible slot claim
        until release; concurrent census sees it."""
        _settings(monkeypatch, max_live=3)
        real_census = spawn_gate.census
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        guard = spawn_gate.run_gate("one-shot", "headless")
        monkeypatch.setattr(spawn_gate, "census", real_census)
        assert spawn_gate.census().slot_claims == 1
        guard.release()
        assert spawn_gate.census().slot_claims == 0

    def test_cap_two_refuses_third_but_not_codex_then_exit_releases_slot(
        self, monkeypatch, capsys
    ):
        _settings(monkeypatch, max_live=99, max_lanes={"zai": 2})
        rows: list[AgentEntry] = []
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, _start: bool(pid))
        monkeypatch.setattr(
            spawn_gate, "_acquire_gate_mutex", lambda _holder, **_kwargs: True
        )
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        for index in (1, 2):
            guard = spawn_gate.run_gate(f"zai-{index}", "pane", route_provider="zai")
            rows.append(
                AgentEntry(
                    name=f"zai-{index}", harness="claude", provider="zai",
                    cwd="/tmp", log_path="/tmp/log", pid=100 + index,
                    pid_start_time=1000 + index,
                )
            )
            guard.release()

        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("zai-3", "pane", route_provider="zai")
        assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP
        refused = capsys.readouterr().err
        assert "provider zai" in refused and "cap 2" in refused
        assert "current count 2" in refused and "queued" not in refused

        codex = spawn_gate.run_gate("codex-peer", "pane")
        codex.release()
        rows[0].status = "exited"
        admitted = spawn_gate.run_gate("zai-4", "pane", route_provider="zai")
        admitted.release()

    def test_unreadable_provider_count_refuses_instead_of_zero(
        self, monkeypatch, capsys
    ):
        _settings(monkeypatch, max_live=99, max_lanes={"zai": 2})
        monkeypatch.setattr(
            spawn_gate, "_acquire_gate_mutex", lambda _holder, **_kwargs: True
        )

        def unreadable():
            raise OSError("registry denied")

        monkeypatch.setattr("fno.agents.registry.load_registry", unreadable)
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("zai-1", "pane", route_provider="zai")
        assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP
        refused = capsys.readouterr().err
        assert "provider zai" in refused and "cap 2" in refused
        assert "current count unavailable" in refused

    def test_partial_forward_registry_refuses_instead_of_undercounting(
        self, monkeypatch
    ):
        from fno.agents.registry import LoadedRegistry

        monkeypatch.setattr(
            "fno.agents.registry.load_registry",
            lambda: LoadedRegistry([], complete=False),
        )
        with pytest.raises(spawn_gate.ProviderCountUnavailable):
            spawn_gate.provider_live_count("zai")

    def test_null_provider_live_row_warns_and_skips(
        self, monkeypatch, capsys
    ):
        row = AgentEntry(
            name="unattributed-live",
            harness="claude",
            provider=None,
            origin="spawn",
            cwd="/tmp",
            log_path="/tmp/log",
            pid=101,
            pid_start_time=1001,
        )
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _start: True)
        assert spawn_gate.provider_live_count("zai") == 0
        assert (
            "1 live row(s) were minted without a provider stamp "
            "(harness=claude, origin=spawn)"
        ) in capsys.readouterr().err

    def test_missing_claim_provider_warns_and_skips(
        self, tmp_path, monkeypatch, capsys
    ):
        claims = tmp_path / ".fno" / "claims"
        claims.mkdir(parents=True)
        (claims / "worker%3Aplain-peer.lock").write_text("claim")
        monkeypatch.setattr(spawn_gate, "_gate_claims_root", lambda: tmp_path)
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        monkeypatch.setattr(
            "fno.claims.core.claim_status",
            lambda *_args, **_kwargs: {"state": "live", "metadata": {}},
        )

        assert spawn_gate.provider_live_count("zai") == 0
        assert (
            "live worker reservation worker:plain-peer was minted without "
            "model_provider; skipping"
        ) in capsys.readouterr().err

    def test_every_registry_mint_site_stamps_model_provider(self):
        root = Path(__file__).resolve().parents[3]
        claude_rust = (root / "crates/fno-agents/src/claude_ask.rs").read_text()
        adopt_rust = (root / "crates/fno-agents/src/claude_adopt.rs").read_text()
        codex_rust = (root / "crates/fno-agents/src/codex_ask.rs").read_text()
        dispatch_python = (root / "cli/src/fno/agents/dispatch.py").read_text()
        mux_python = (root / "cli/src/fno/agents/mux_spawn.py").read_text()

        def section(text: str, start: str, end: str) -> str:
            assert start in text and end in text.split(start, 1)[1]
            return text.split(start, 1)[1].split(end, 1)[0]

        rust_claude_spawn = section(
            claude_rust, "fn create(\n", "#[cfg(test)]"
        )
        rust_claude_adopt = section(
            adopt_rust, "pub fn mint_adopted_entry", "pub fn upsert_adopted_row"
        )
        rust_codex_create = section(
            codex_rust, "fn dispatch_create(\n", "fn dispatch_resume(\n"
        )
        python_codex_create = section(
            dispatch_python, "def _codex_create_path(\n", "def _codex_followup_path(\n"
        )
        python_claude_spawn = section(
            dispatch_python, "def _claude_create_path(\n", "def dispatch_ask(\n"
        )
        assert "def dispatch_spawn_pane(\n" in mux_python
        python_pane_spawn = mux_python.split("def dispatch_spawn_pane(\n", 1)[1]

        assert 'provider: Some("anthropic".to_string())' in rust_claude_spawn
        assert 'provider: Some("anthropic".into())' in rust_claude_adopt
        assert 'provider: Some("openai".to_string())' in rust_codex_create
        assert 'provider="openai"' in python_codex_create
        assert (
            "lane_provider = route_provider or resolve_lane_vendor("
            in python_claude_spawn
        )
        assert "provider=lane_provider" in python_claude_spawn
        assert "provider=resolved_lane_provider" in python_pane_spawn

    def test_every_registry_mint_site_stamps_spawned_by(self):
        """The parent-edge sibling of the provider stamp test: each mint site
        calls the ambient capture helper and wires the triple onto the row.
        The shell gate (check-spawn-lineage-parity.sh) runs the same sweep in
        CI; this pytest copy fails in the local suite a developer actually
        runs."""
        root = Path(__file__).resolve().parents[3]
        rust_sites = {
            "claude create": (
                root / "crates/fno-agents/src/claude_ask.rs", "fn create(\n", "#[cfg(test)]"
            ),
            "claude adopt": (
                root / "crates/fno-agents/src/claude_adopt.rs",
                "pub fn mint_adopted_entry", "pub fn upsert_adopted_row",
            ),
            "codex create": (
                root / "crates/fno-agents/src/codex_ask.rs",
                "fn dispatch_create(\n", "fn dispatch_resume(\n",
            ),
            "gemini create": (
                root / "crates/fno-agents/src/gemini_ask.rs",
                "fn dispatch_create(\n", "fn dispatch_resume(\n",
            ),
            "daemon stream worker": (
                root / "crates/fno-agents/src/daemon.rs",
                "fn build_claude_stream_entry", "fn acquire_session_claim",
            ),
            "synthesized row": (
                root / "crates/fno-agents/src/client_verbs.rs",
                "fn mint_synthesized_entry", "fn upsert_synthesized_row",
            ),
        }
        for label, (path, start, end) in rust_sites.items():
            text = path.read_text()
            assert start in text and end in text.split(start, 1)[1], label
            body = text.split(start, 1)[1].split(end, 1)[0]
            assert "crate::claims::ambient_parent_edge()" in body, label
            assert "spawned_by_session: parent_session" in body, label

        state_rust = (root / "crates/fno-agents/src/state.rs").read_text()
        for field in ("spawned_by_session", "spawned_by_harness", "spawned_by_cwd"):
            assert f"pub {field}:" in state_rust, field

        dispatch_python = (root / "cli/src/fno/agents/dispatch.py").read_text()
        codex_create = dispatch_python.split("def _codex_create_path(\n", 1)[1].split(
            "def _codex_followup_path(\n", 1
        )[0]
        assert "_capture_parent_edge()" in codex_create
        assert "spawned_by_session=_cx_session" in codex_create
        registry_python = (root / "cli/src/fno/agents/registry.py").read_text()
        register = registry_python.split("def register_existing_session(", 1)[1].split(
            "def restamp_harness_session_id(", 1
        )[0]
        assert 'origin == "operator"' in register
        assert "spawned_by_session=_sb_session" in register
        fallback_python = (root / "cli/src/fno/agents/store_fallback.py").read_text()
        assert "spawned_by_session=_sb_session" in fallback_python

    def test_force_does_not_bypass_provider_cap(self, monkeypatch):
        _settings(monkeypatch, max_live=99, max_lanes={"zai": 1})
        row = AgentEntry(
            name="zai-live", harness="claude", provider="zai", cwd="/tmp",
            log_path="/tmp/log", pid=101, pid_start_time=1001,
        )
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _start: True)
        monkeypatch.setattr(
            spawn_gate, "_acquire_gate_mutex", lambda _holder, **_kwargs: True
        )
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate(
                "zai-forced", "pane", route_provider="zai", force=True
            )
        assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP

    def test_provider_cap_never_queues_on_a_busy_mutex(self, monkeypatch, capsys):
        _settings(monkeypatch, max_live=99, max_lanes={"zai": 2})
        monkeypatch.setattr(
            spawn_gate, "_acquire_gate_mutex", lambda _holder, **_kwargs: False
        )
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("zai-now", "pane", route_provider="zai")
        assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP
        refused = capsys.readouterr().err
        assert "current count unavailable" in refused and "queued" not in refused

    def test_provider_count_requires_positive_liveness_and_skips_exited(
        self, monkeypatch
    ):
        rows = [
            AgentEntry(name="positive", harness="claude", provider="zai", cwd="/tmp", log_path="/tmp/log", pid=101, pid_start_time=1001),
            AgentEntry(name="stale", harness="claude", provider="zai", cwd="/tmp", log_path="/tmp/log", pid=102, pid_start_time=1002),
            AgentEntry(name="exited", harness="claude", provider="zai", cwd="/tmp", log_path="/tmp/log", status="exited", pid=103, pid_start_time=1003),
        ]
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        monkeypatch.setattr(
            spawn_gate, "_pid_alive",
            lambda pid, _start: {101: True, 102: False, 103: True}[pid],
        )
        assert spawn_gate.provider_live_count("zai") == 1

    def test_live_pid_without_incarnation_token_refuses_but_dead_pid_skips(
        self, monkeypatch
    ):
        row = AgentEntry(
            name="ambiguous", harness="claude", provider="zai", cwd="/tmp",
            log_path="/tmp/log", pid=101,
        )
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _start: True)
        with pytest.raises(spawn_gate.ProviderCountUnavailable):
            spawn_gate.provider_live_count("zai")

        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda _pid, _start: False)
        assert spawn_gate.provider_live_count("zai") == 0

    def test_headless_provider_claim_holds_and_releases_a_lane(
        self, monkeypatch, capsys
    ):
        _settings(monkeypatch, max_live=99, max_lanes={"zai": 2})
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )

        first = spawn_gate.run_gate("peer-1", "headless", route_provider="zai")
        second = spawn_gate.run_gate("peer-2", "headless", route_provider="zai")
        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("peer-3", "headless", route_provider="zai")
        assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP
        assert "current count 2" in capsys.readouterr().err

        first.release()
        replacement = spawn_gate.run_gate(
            "peer-4", "headless", route_provider="zai"
        )
        replacement.release()
        second.release()

    def test_known_unrouted_headless_claim_does_not_block_capped_provider(
        self, monkeypatch
    ):
        _settings(monkeypatch, max_live=99, max_lanes={"zai": 2})
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )

        unrelated = spawn_gate.run_gate("codex-peer", "headless")
        assert spawn_gate.provider_live_count("zai") == 0
        unrelated.release()

    def test_suspect_claim_for_other_provider_does_not_block_count(
        self, tmp_path, monkeypatch
    ):
        claims = tmp_path / ".fno" / "claims"
        claims.mkdir(parents=True)
        (claims / "worker%3Aopenai-peer.lock").write_text("claim")
        monkeypatch.setattr(spawn_gate, "_gate_claims_root", lambda: tmp_path)
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        other_provider = "openai"
        monkeypatch.setattr(
            "fno.claims.core.claim_status",
            lambda *_args, **_kwargs: {
                "state": "suspect",
                "metadata": {"model_provider": other_provider},
            },
        )

        assert spawn_gate.provider_live_count("zai") == 0

    def test_capped_headless_refuses_when_lane_reservation_is_unwritable(
        self, monkeypatch, capsys
    ):
        _settings(monkeypatch, max_live=99, max_lanes={"zai": 2})
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        monkeypatch.setattr(
            "fno.claims.core.acquire_claim",
            lambda *args, **kwargs: (
                True
                if args[0] == "spawn-gate"
                else (_ for _ in ()).throw(OSError("claim store denied"))
            ),
        )

        with pytest.raises(SystemExit) as exc:
            spawn_gate.run_gate("peer-1", "headless", route_provider="zai")
        assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP
        refused = capsys.readouterr().err
        assert "provider zai" in refused and "cap 2" in refused
        assert "current count unavailable" in refused

    def test_release_failures_are_loud_and_retain_retry_state(
        self, monkeypatch, capsys
    ):
        guard = spawn_gate.GateGuard(
            _gate_holder="gate-holder",
            _worker_key="worker:peer",
            _worker_holder="worker-holder",
        )
        monkeypatch.setattr(
            "fno.claims.core.release_claim",
            lambda *args, **kwargs: (_ for _ in ()).throw(OSError("store denied")),
        )

        guard.release()

        assert guard._gate_holder == "gate-holder"
        assert guard._worker_key == "worker:peer"
        refused = capsys.readouterr().err
        assert "could not release gate mutex" in refused
        assert "could not release worker reservation worker:peer" in refused

    def test_release_retries_transient_claim_store_failures(self, monkeypatch):
        guard = spawn_gate.GateGuard(
            _gate_holder="gate-holder",
            _worker_key="worker:peer",
            _worker_holder="worker-holder",
        )
        attempts: dict[str, int] = {}

        def flaky_release(key, *_args, **_kwargs):
            attempts[key] = attempts.get(key, 0) + 1
            if attempts[key] < 3:
                raise OSError("transient store failure")

        monkeypatch.setattr("fno.claims.core.release_claim", flaky_release)

        guard.release()

        assert attempts == {"spawn-gate": 3, "worker:peer": 3}
        assert guard._gate_holder is None
        assert guard._worker_key is None

    def test_pidless_bg_requires_a_readable_positive_roster_marker(
        self, tmp_path, monkeypatch
    ):
        row = AgentEntry(
            name="zai-bg", harness="claude", provider="zai", cwd="/tmp",
            log_path="/tmp/log", short_id="11111111",
        )
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
        with pytest.raises(spawn_gate.ProviderCountUnavailable):
            spawn_gate.provider_live_count("zai")
        _write_roster(
            tmp_path,
            {"zai-bg": {"sessionId": "11111111-2222-3333-4444-55555555abcd", "pid": ALIVE}},
        )
        assert spawn_gate.provider_live_count("zai") == 1


class TestQos:
    def test_wrap_identity_when_off(self, monkeypatch):
        monkeypatch.setattr(spawn_gate, "_qos_enabled", lambda: False)
        argv = ["sh", "-c", "true"]
        assert spawn_gate.qos_wrap(argv) == argv

    def test_wrap_prefixes_platform_demotion(self, monkeypatch):
        """AC3-HP: utility wraps the exec (absolute wrapper path)."""
        monkeypatch.setattr(spawn_gate, "_qos_enabled", lambda: True)
        wrapped = spawn_gate.qos_wrap(["sh", "-c", "true"])
        import os as _os
        import sys as _sys

        if _sys.platform == "darwin" and _os.path.exists("/usr/sbin/taskpolicy"):
            assert wrapped[:4] == ["/usr/sbin/taskpolicy", "-c", "utility", "--"]
            assert wrapped[4:] == ["sh", "-c", "true"]
        elif _sys.platform.startswith("linux") and _os.path.exists("/usr/bin/nice"):
            assert wrapped[:3] == ["/usr/bin/nice", "-n", "10"]

    def test_wrap_skips_unresolvable_command(self, monkeypatch):
        """A missing provider CLI must surface its own NotFound, unwrapped."""
        monkeypatch.setattr(spawn_gate, "_qos_enabled", lambda: True)
        ghost = ["definitely-not-a-real-cli-xyz"]
        assert spawn_gate.qos_wrap(ghost) == ghost

    def test_demote_failure_is_nonfatal_warning(self, monkeypatch, capsys):
        """AC3-ERR: taskpolicy failure warns once, never raises."""
        monkeypatch.setattr(spawn_gate, "_qos_enabled", lambda: True)
        import subprocess

        def boom(*a, **k):
            raise FileNotFoundError("taskpolicy not found")

        monkeypatch.setattr(subprocess, "run", boom)
        spawn_gate.qos_demote_pid(12345)
        assert "non-fatal" in capsys.readouterr().err

    def test_bg_demotion_bounded_when_pid_never_appears(
        self, tmp_path, monkeypatch, capsys
    ):
        """AC3-UI: pid never in roster -> one warning, nothing blocks."""
        monkeypatch.setattr(spawn_gate, "_qos_enabled", lambda: True)
        spawn_gate.qos_demote_bg_worker("deadbeef", poll_s=0.05)
        assert "QoS demotion skipped" in capsys.readouterr().err
