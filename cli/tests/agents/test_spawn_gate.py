"""Spawn gate (x-c5cc): union live-count, RAM floor, queue loop, QoS wrap.

FNO_THINK_SPAWN=0 discipline is irrelevant here (nothing dispatches), but
every test redirects FNO_CLAUDE_DAEMON_DIR + FNO_CLAIMS_ROOT so no real
roster or claims dir is touched.
"""
from __future__ import annotations

import json
import os

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


def _row(name: str, *, status="live", pid=None, claude_short_id=None, short_id=""):
    return AgentEntry(
        name=name,
        provider="claude",
        cwd="/tmp",
        log_path="/tmp/log",
        status=status,
        pid=pid,
        claude_short_id=claude_short_id,
        short_id=short_id,
    )


ALIVE = os.getpid()  # a pid that is definitely alive (this test process)


class TestCensus:
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
            _row("adopted", pid=ALIVE, claude_short_id="bbbbbbbb"),  # dup of roster
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


def _settings(monkeypatch, *, max_live=3, min_free_gb=0.0):
    """Point run_gate at fixed knobs without touching real settings."""

    class _A:
        pass

    a = _A()
    a.max_live = max_live
    a.min_free_gb = min_free_gb

    class _S:
        pass

    s = _S()
    s.agents = a
    monkeypatch.setattr("fno.config.load_settings", lambda: s)


class TestRunGate:
    def test_under_cap_passes_silently(self, monkeypatch, capsys):
        """AC1-HP: nothing on stderr, no queue, guard holds the mutex."""
        _settings(monkeypatch, max_live=3)
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
        )
        guard = spawn_gate.run_gate("w2", "bg")
        assert capsys.readouterr().err == ""
        guard.release()

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
        """AC1-ERR: timeout exit code is the gate's own, message names top."""
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
        err = capsys.readouterr().err
        assert "fno agents top" in err
        assert exc.value.code not in (2, 13, 14, 15, 18, 127)

    def test_force_bypasses_and_prints_forced_line(self, monkeypatch, capsys):
        _settings(monkeypatch, max_live=1)
        w = spawn_gate.LiveWorker("fno", "w1", "claude", "bg", ALIVE, "busy")
        monkeypatch.setattr(
            spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[w], fno_slot_workers=1)
        )
        guard = spawn_gate.run_gate("w2", "bg", force=True)
        assert "forced past cap" in capsys.readouterr().err
        guard.release()

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
