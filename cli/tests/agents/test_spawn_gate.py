"""Spawn gate (x-c5cc): union live-count, RAM floor, queue loop, QoS wrap.

FNO_THINK_SPAWN=0 discipline is irrelevant here (nothing dispatches), but
every test redirects FNO_CLAUDE_DAEMON_DIR + FNO_CLAIMS_ROOT so no real
roster or claims dir is touched.
"""
from __future__ import annotations

import json
import os
import time
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


@pytest.fixture(autouse=True)
def _no_live_cpu_axis(monkeypatch):
    """The CPU axis is stubbed admitting unless a test asks otherwise.

    Since x-7783 every spawn takes the footprint reading, and that read is a
    ps snapshot against the real machine. Left unstubbed these tests refuse
    or hold according to what the developer's box happens to be doing. The
    prefetch is pinned too: run_gate takes the read before the decider, and
    an unpinned read would price a real ps snapshot per pass.
    """
    from fno import doctor_footprint
    from fno.footprint import Admission, Footprint

    idle = Footprint(0.0, 0.0, 0.1, 0, 0, 0, 0, 0.0, 0.2, [], 0, None)
    monkeypatch.setattr(
        spawn_gate, "_prefetch_fleet_reading", lambda: (idle, None)
    )
    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (0.5, 40.0))
    admit = Admission(
        verdict="admit",
        axis="fleet_cpu_share",
        reason="test admit",
        share_low=0.1,
        share_high=0.1,
        bound="exact",
        fleet_cores=1.2,
        machine_cores=6.0,
        capacity_cores=12.0,
        ceiling=0.5,
        gap=None,
        load_15m=1.0,
        backstop=480.0,
    )
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: admit)


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


def _load(load1: float):
    return lambda: (load1, 0.0, 0.0)


def test_load_snapshot_is_display_only(monkeypatch):
    """x-7783: the load reading is a trend line. Nothing on it gates."""
    monkeypatch.setattr(spawn_gate.os, "getloadavg", _load(141.6))
    monkeypatch.setattr(spawn_gate, "_load_cpus", lambda: 12)

    snapshot = spawn_gate._load_snapshot(8.0)

    assert snapshot.load_1m == pytest.approx(141.6)
    assert snapshot.load_cpu_count == 12
    assert snapshot.load_15m is not None


def test_load_snapshot_marks_unreadable_load(monkeypatch):
    def boom():
        raise OSError("no loadavg here")

    monkeypatch.setattr(spawn_gate.os, "getloadavg", boom)
    monkeypatch.setattr(spawn_gate, "_load_cpus", lambda: 12)

    snapshot = spawn_gate._load_snapshot(8.0)

    assert snapshot.load_1m is None


ALIVE = os.getpid()  # a pid that is definitely alive (this test process)


class TestCensus:
    def test_succession_keeps_one_positive_slot_and_current_address(self, monkeypatch):
        """AC5-HP/AC6-HP: one row remains one slot while its address advances."""
        from fno.agents.registry import resolve_agent_in

        row = _row("target-worker", pid=ALIVE, short_id="old-session")
        row.mux = {"session": "main", "pane_id": 12}
        row.harness_session_id = "old-session"
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
        before = spawn_gate.census()

        row.harness_session_id = "new-session"
        row.predecessor_session_ids = ["old-session"]
        after = spawn_gate.census()
        resolved = resolve_agent_in([row], "new-session")

        assert before.fno_slot_workers == 1
        assert after.fno_slot_workers == 1
        assert resolved.entry.harness_session_id == "new-session"
        assert resolved.entry.mux == {"session": "main", "pane_id": 12}

    def test_duplicate_successor_is_an_explicit_address_ambiguity(self):
        """AC5-ERR: two rows carrying B cannot resolve as one worker."""
        from fno.agents.registry import AgentResolutionError, resolve_agent_in

        rows = [
            _row("target-a", pid=ALIVE, short_id="new-session"),
            _row("target-b", pid=ALIVE, short_id="new-session"),
        ]
        for row in rows:
            row.harness_session_id = "new-session"

        with pytest.raises(AgentResolutionError, match="ambiguous"):
            resolve_agent_in(rows, "new-session")

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

    def test_spawning_outlived_by_a_live_pid_renders_quiet_with_basis(
        self, monkeypatch
    ):
        """(x-d401 / x-0248) AC3-HP: a stored `spawning` token a live pid has
        outlived does not render a bare `spawning` - the row names the
        movement-derived state and a basis for the rewrite. The process is
        confirmed but the transcript is unread, so the served word is
        `quiet`, never a `live` token. AC3-EDGE: a row with no pid recorded
        yet keeps its honest token."""
        from datetime import datetime, timedelta, timezone

        stale = datetime.now(timezone.utc) - timedelta(hours=13)
        fresh = datetime.now(timezone.utc) - timedelta(seconds=5)
        rows = [
            _row("stale-spawn", status="spawning", pid=ALIVE),
            _row("fresh-spawn", status="spawning", pid=ALIVE),
        ]
        rows[0].created_at = stale.strftime("%Y-%m-%dT%H:%M:%SZ")
        rows[1].created_at = fresh.strftime("%Y-%m-%dT%H:%M:%SZ")
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)

        by_name = {w.name: w for w in spawn_gate.census().workers}

        assert by_name["stale-spawn"].status == "quiet", (
            "a working row must not read spawning"
        )
        assert by_name["stale-spawn"].status_basis == "stale-spawning-live-pid"
        assert by_name["fresh-spawn"].status == "spawning", (
            "a live pid younger than the spawn timeout is still mid-spawn"
        )
        assert by_name["fresh-spawn"].status_basis is None

    def test_unreadable_pid_never_upgrades_to_positive_liveness(self, monkeypatch):
        """(x-d401) An UNMEASURED pid must not fire the stale-spawning rule.

        `resolve_session_pid` falls back to the recorded pid, so a bare
        `session_pid is not None` is true for any row carrying a pid. Paired
        with an unreadable incarnation, that handed the rule positive liveness
        it never measured, and a parked `spawning` row was rewritten to `live`
        under a basis naming a measurement nobody took. The census warns
        "process incarnation unreadable" for exactly these rows, so asserting
        liveness three lines later contradicts its own warning.
        """
        from datetime import datetime, timedelta, timezone

        stale = datetime.now(timezone.utc) - timedelta(hours=13)
        rows = [_row("unreadable-spawn", status="spawning", pid=ALIVE)]
        rows[0].created_at = stale.strftime("%Y-%m-%dT%H:%M:%SZ")
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        # None = "could not read this incarnation", the case the warning names.
        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_a, **_k: None)

        by_name = {w.name: w for w in spawn_gate.census().workers}

        assert by_name["unreadable-spawn"].status == "spawning", (
            "unknown liveness keeps the token, per the rule's own contract"
        )
        assert by_name["unreadable-spawn"].status_basis is None, (
            "no basis may name a measurement that was never taken"
        )

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


class TestCensus:
    def test_succession_keeps_one_positive_slot_and_current_address(self, monkeypatch):
        """AC5-HP/AC6-HP: one row remains one slot while its address advances."""
        from fno.agents.registry import resolve_agent_in

        row = _row("target-worker", pid=ALIVE, short_id="old-session")
        row.mux = {"session": "main", "pane_id": 12}
        row.harness_session_id = "old-session"
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
        before = spawn_gate.census()

        row.harness_session_id = "new-session"
        row.predecessor_session_ids = ["old-session"]
        after = spawn_gate.census()
        resolved = resolve_agent_in([row], "new-session")

        assert before.fno_slot_workers == 1
        assert after.fno_slot_workers == 1
        assert resolved.entry.harness_session_id == "new-session"
        assert resolved.entry.mux == {"session": "main", "pane_id": 12}

    def test_duplicate_successor_is_an_explicit_address_ambiguity(self):
        """AC5-ERR: two rows carrying B cannot resolve as one worker."""
        from fno.agents.registry import AgentResolutionError, resolve_agent_in

        rows = [
            _row("target-a", pid=ALIVE, short_id="new-session"),
            _row("target-b", pid=ALIVE, short_id="new-session"),
        ]
        for row in rows:
            row.harness_session_id = "new-session"

        with pytest.raises(AgentResolutionError, match="ambiguous"):
            resolve_agent_in(rows, "new-session")

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

    def test_spawning_outlived_by_a_live_pid_renders_quiet_with_basis(
        self, monkeypatch
    ):
        """(x-d401 / x-0248) AC3-HP: a stored `spawning` token a live pid has
        outlived does not render a bare `spawning` - the row names the
        movement-derived state and a basis for the rewrite. The process is
        confirmed but the transcript is unread, so the served word is
        `quiet`, never a `live` token. AC3-EDGE: a row with no pid recorded
        yet keeps its honest token."""
        from datetime import datetime, timedelta, timezone

        stale = datetime.now(timezone.utc) - timedelta(hours=13)
        fresh = datetime.now(timezone.utc) - timedelta(seconds=5)
        rows = [
            _row("stale-spawn", status="spawning", pid=ALIVE),
            _row("fresh-spawn", status="spawning", pid=ALIVE),
        ]
        rows[0].created_at = stale.strftime("%Y-%m-%dT%H:%M:%SZ")
        rows[1].created_at = fresh.strftime("%Y-%m-%dT%H:%M:%SZ")
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)

        by_name = {w.name: w for w in spawn_gate.census().workers}

        assert by_name["stale-spawn"].status == "quiet", (
            "a working row must not read spawning"
        )
        assert by_name["stale-spawn"].status_basis == "stale-spawning-live-pid"
        assert by_name["fresh-spawn"].status == "spawning", (
            "a live pid younger than the spawn timeout is still mid-spawn"
        )
        assert by_name["fresh-spawn"].status_basis is None

    def test_unreadable_pid_never_upgrades_to_positive_liveness(self, monkeypatch):
        """(x-d401) An UNMEASURED pid must not fire the stale-spawning rule.

        `resolve_session_pid` falls back to the recorded pid, so a bare
        `session_pid is not None` is true for any row carrying a pid. Paired
        with an unreadable incarnation, that handed the rule positive liveness
        it never measured, and a parked `spawning` row was rewritten to `live`
        under a basis naming a measurement nobody took. The census warns
        "process incarnation unreadable" for exactly these rows, so asserting
        liveness three lines later contradicts its own warning.
        """
        from datetime import datetime, timedelta, timezone

        stale = datetime.now(timezone.utc) - timedelta(hours=13)
        rows = [_row("unreadable-spawn", status="spawning", pid=ALIVE)]
        rows[0].created_at = stale.strftime("%Y-%m-%dT%H:%M:%SZ")
        monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
        # None = "could not read this incarnation", the case the warning names.
        monkeypatch.setattr(spawn_gate, "_pid_alive", lambda *_a, **_k: None)

        by_name = {w.name: w for w in spawn_gate.census().workers}

        assert by_name["unreadable-spawn"].status == "spawning", (
            "unknown liveness keeps the token, per the rule's own contract"
        )
        assert by_name["unreadable-spawn"].status_basis is None, (
            "no basis may name a measurement that was never taken"
        )

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




def _stub_verb(monkeypatch, answer=None, error=None):
    """Stub the gate verb: a fixed JSON answer, or a failure."""
    from fno import rust_binary

    if error is not None:
        def boom(*_a, **_k):
            raise error
        monkeypatch.setattr(rust_binary, "verb_call", boom)
        return boom

    def fake(verb, payload, **_kwargs):
        assert verb == "spawn-gate", f"the transport must call the gate verb, got {verb}"
        fake.payload = payload
        return answer

    fake.payload = None
    monkeypatch.setattr(rust_binary, "verb_call", fake)
    return fake


class TestTransport:
    """The transport contract: run_gate asks the ONE gate verb and carries
    the answer out; the axes themselves are pinned in Rust."""

    def test_admitted_answer_becomes_a_guard_holding_the_verbs_keys(self, monkeypatch):
        stub = _stub_verb(
            monkeypatch,
            {
                "status": "admitted",
                "gate_key": "gate:spawn",
                "gate_holder": "spawn-gate:4242:w1",
                "worker_key": None,
                "worker_holder": None,
            },
        )
        guard = spawn_gate.run_gate("w1", "bg")
        assert guard._gate_holder == "spawn-gate:4242:w1"
        assert guard._worker_key is None
        assert guard._route_provider is None
        assert guard._spawn_name == "w1"
        assert guard._substrate == "bg"
        # The caller's pid travels in the payload: the verb's claims are
        # owned by the CALLER across the process boundary (locked decision 3).
        assert stub.payload["holder_pid"] == os.getpid()
        assert stub.payload["mode"] == "gate"

    def test_refused_answer_raises_gate_refused_with_the_receipt(self, monkeypatch, capsys):
        events = []
        monkeypatch.setattr(
            spawn_gate, "_emit_gate_event", lambda kind, **data: events.append((kind, data))
        )
        receipt = {
            "status": "refused",
            "reason": "provider_cap",
            "provider": "zai",
            "cap": 5,
            "count": 5,
            "current_count": 5,
        }
        _stub_verb(
            monkeypatch,
            {
                "status": "refused",
                "exit_code": spawn_gate.EXIT_PROVIDER_CAP,
                "receipt": receipt,
                "event": {"reason": "provider_cap", "provider": "zai"},
            },
        )
        with pytest.raises(spawn_gate.GateRefused) as exc:
            spawn_gate.run_gate("w", "pane", route_provider="zai")
        assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP
        assert exc.value.receipt == receipt
        # Exactly ONE refusal event lands, carrying the receipt fields.
        kinds = [k for k, _ in events]
        assert kinds == ["spawn_gate_refused"]
        data = events[0][1]
        assert data["exit_code"] == spawn_gate.EXIT_PROVIDER_CAP
        assert data["reason"] == "provider_cap"
        assert data["provider"] == "zai"
        assert data["gate"] == "python"
        assert data["name"] == "w"
        assert data["substrate"] == "pane"

    def test_missing_binary_refuses_exit_86_gate_unavailable(self, monkeypatch, capsys):
        from fno.rust_binary import VerbUnavailable

        events = []
        monkeypatch.setattr(
            spawn_gate, "_emit_gate_event", lambda kind, **data: events.append((kind, data))
        )
        _stub_verb(monkeypatch, error=VerbUnavailable("the fno-agents binary was not found"))
        with pytest.raises(spawn_gate.GateRefused) as exc:
            spawn_gate.run_gate("w", "pane")
        assert exc.value.code == spawn_gate.EXIT_GATE_UNAVAILABLE
        assert exc.value.receipt["reason"] == "gate_unavailable"
        assert "not found" in exc.value.receipt["error"]
        assert [k for k, _ in events] == ["spawn_gate_refused"]

    def test_the_callers_session_rides_the_payload(self, monkeypatch):
        stub = _stub_verb(monkeypatch, {"status": "admitted"})
        spawn_gate.run_gate("w", "bg")
        assert "caller_session" in stub.payload
        assert stub.payload["account"] is None

    def test_probe_is_a_verb_call_passed_through(self, monkeypatch):
        stub = _stub_verb(
            monkeypatch, {"verdict": "accepted", "lanes": {}, "live_workers": 0}
        )
        answer = spawn_gate.probe_capacity()
        assert answer["verdict"] == "accepted"
        assert stub.payload["mode"] == "probe"

    def test_probe_never_raises_on_an_unanswered_gate(self, monkeypatch):
        from fno.rust_binary import VerbUnavailable

        _stub_verb(monkeypatch, error=VerbUnavailable("exited 1"))
        answer = spawn_gate.probe_capacity()
        assert answer["verdict"] == "unknown"
        assert answer["reason"] == "gate_unavailable"
        assert "exited 1" in answer["error"]

    def test_gate_unavailable_refusal_event_names_the_reason(self, monkeypatch):
        from fno.rust_binary import VerbUnavailable

        events = []
        monkeypatch.setattr(
            spawn_gate, "_emit_gate_event", lambda kind, **data: events.append((kind, data))
        )
        _stub_verb(monkeypatch, error=VerbUnavailable("boom"))
        with pytest.raises(spawn_gate.GateRefused):
            spawn_gate.run_gate("w", "headless")
        data = events[0][1]
        assert data["reason"] == "gate_unavailable"
        assert data["exit_code"] == spawn_gate.EXIT_GATE_UNAVAILABLE
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


# --- parent-edge visibility (x-7b36 change 12) --------------------------------
# A null spawned_by_session is sometimes correct (an ambiguous identity resolve
# records no lineage rather than a wrong one); the defect was its SILENCE. The
# receipt line names it at spawn time, and the list row carries the field so
# lineage questions debugged through `agents list -J` measure something.


class TestParentEdgeNotice:
    def test_parent_edge_notice_names_reason_and_orphan_check(
        self, monkeypatch, capsys
    ):
        from fno.agents.dispatch import _report_unlinked_parent
        from fno.harness_identity import OwnedHarnessIdentity

        monkeypatch.setattr(
            "fno.claims.self_identity.resolve_self_identity",
            lambda: OwnedHarnessIdentity(
                session_id=None,
                harness=None,
                markers_present=(
                    ("CLAUDE_CODE_SESSION_ID", "claude", "x"),
                    ("CODEX_THREAD_ID", "codex", "y"),
                ),
                disposition="ambiguous",
            ),
        )
        _report_unlinked_parent(None)
        err = capsys.readouterr().err
        assert "parent edge NOT recorded" in err
        assert "disposition=ambiguous" in err
        assert "CLAUDE_CODE_SESSION_ID" in err
        # The consequence, not just the cause: the spawner learns the child is
        # invisible to its orphan check.
        assert "will not appear in its spawner's orphan check" in err

    def test_parent_edge_notice_silent_when_parent_recorded(
        self, monkeypatch, capsys
    ):
        from fno.agents.dispatch import _report_unlinked_parent

        _report_unlinked_parent("d88ad3a3-b820-440e-9654-70fad39cd7d8")
        assert capsys.readouterr().err == ""

    def test_spawned_by_session_rides_list_row(self):
        from fno.agents.format import serialize_entry

        entry = AgentEntry(
            name="linked-worker",
            cwd="/w",
            log_path="/l",
            harness="claude",
            spawned_by_session="d88ad3a3-b820-440e-9654-70fad39cd7d8",
        )
        row = serialize_entry(entry, live_status=None)
        # Same value registry-json reports for that row (AC30), and the key is
        # present even when null so declared-but-unwritten and dropped stay
        # distinguishable.
        assert (
            row["spawned_by_session"] == "d88ad3a3-b820-440e-9654-70fad39cd7d8"
        )
        null_row = serialize_entry(
            AgentEntry(name="bare", cwd="/w", log_path="/l", harness="claude"),
            live_status=None,
        )
        assert "spawned_by_session" in null_row
        assert null_row["spawned_by_session"] is None


# --- the crown types the verb (x-7b36 change 11) ------------------------------


class TestReignTyped:
    def test_crowned_spawn_payload_opens_with_the_verb(self):
        from fno.agents.dispatch import _reign_typed_message

        message, typed = _reign_typed_message("run the territory", 2, "epic-x", False)
        assert typed is True
        assert message.splitlines()[0] == "/fno:reign epic-x"
        # The operator's brief follows the verb as the payload body.
        assert message.splitlines()[1] == "run the territory"

    def test_uncrowned_and_revived_spawns_keep_their_payload(self):
        from fno.agents.dispatch import _reign_typed_message

        assert _reign_typed_message("brief", None, None, False) == ("brief", False)
        assert _reign_typed_message("brief", 2, "epic-x", True) == ("brief", False)


# ---------------------------------------------------------------------------
# AC3-HP/EDGE: the durable fleet incident gate (x-77db)
# ---------------------------------------------------------------------------

def _write_incident_state(tmp_path: Path, state: str) -> Path:
    """Point FNO_AGENTS_HOME at a home whose fleet-stop.json holds `state`."""
    home = tmp_path / ".fno" / "agents"
    home.mkdir(parents=True, exist_ok=True)
    (home / "fleet-stop.json").write_text(json.dumps({
        "version": 1,
        "state": state,
        "generation": 3,
        "changed_at": "2026-09-11T00:00:00Z",
        "changed_by": "op",
        "reason": "wedged lock",
    }))
    return home


