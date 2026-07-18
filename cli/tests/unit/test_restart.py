"""Unit tests for `fno restart` (x-69b3)."""
from __future__ import annotations

import json
import types
from pathlib import Path

from typer.testing import CliRunner

from fno import restart
from fno.cli import app

runner = CliRunner()


def _fake_daemon_binary(monkeypatch, path: str = "/cargo/bin/fno-agents") -> None:
    from fno.agents import rust_runtime

    monkeypatch.setattr(rust_runtime, "resolve_installed_binary", lambda: Path(path))


def _record_run(calls: list) -> object:
    def _run(cmd, **kwargs):
        calls.append(list(cmd))
        return types.SimpleNamespace(returncode=0, stdout="", stderr="")

    return _run


def _rows_seq(monkeypatch, seq: list[list[dict]]) -> None:
    """_agents_rows returns each list in `seq` in turn (last one repeats)."""
    it = iter(seq)
    monkeypatch.setattr(restart, "_agents_rows", lambda: next(it, seq[-1]))


def test_restart_restarts_daemon_and_reports_mux(monkeypatch) -> None:
    """Default: restart the daemon, REPORT (not kill) live mux sessions."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "main", "state": "live"}])

    result = runner.invoke(app, ["restart"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno-agents", "restart"] in calls
    assert not any("kill-server" in c for c in calls), "must NOT kill mux without --mux"
    assert "live mux session" in result.output


def test_restart_mux_flag_kills_each_session(monkeypatch) -> None:
    """--mux: kill each live mux session so it respawns on the new binary."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [{"session": "main", "state": "live"}, {"session": "work", "state": "live"}],
    )

    result = runner.invoke(app, ["restart", "--mux"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno", "mux", "kill-server", "main"] in calls
    assert ["/cargo/bin/fno", "mux", "kill-server", "work"] in calls


def test_restart_mux_skips_non_live_sessions(monkeypatch) -> None:
    """--mux only kills LIVE sessions; stale/unqueryable rows are reported, not
    killed (killing a non-live socket is meaningless)."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [{"session": "live1", "state": "live"}, {"session": "dead", "state": "stale"}],
    )

    result = runner.invoke(app, ["restart", "--mux"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno", "mux", "kill-server", "live1"] in calls
    assert not any("dead" in c for c in calls), "must NOT kill a non-live session"


def test_restart_auto_restarts_stale_wire_server_without_mux(monkeypatch) -> None:
    """x-1a85: a stale-wire live server is auto-restarted even WITHOUT --mux (a
    new client can't attach it anyway); a current-wire server stays opt-in."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [
            {"session": "old", "state": "live", "stale": True},
            {"session": "cur", "state": "live", "stale": False},
        ],
    )

    result = runner.invoke(app, ["restart"])  # NO --mux
    assert result.exit_code == 0
    assert ["/cargo/bin/fno", "mux", "kill-server", "old"] in calls, "stale auto-restarted"
    assert not any(
        "kill-server" in c and "cur" in c for c in calls
    ), "a current-wire server is left opt-in"
    assert "current wire" in result.output, "current-wire server reported, not killed"


def test_restart_daemon_failure_exits_nonzero(monkeypatch) -> None:
    """A real daemon-restart failure fails the command (scripts must see it)."""
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess, "run", lambda cmd, **k: types.SimpleNamespace(returncode=3)
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["restart"])
    assert result.exit_code == 1
    assert "exited 3" in result.output


def test_restart_json_summary(monkeypatch) -> None:
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess, "run", lambda cmd, **k: types.SimpleNamespace(returncode=0)
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "main", "state": "live"}])

    result = runner.invoke(app, ["restart", "--json"])
    assert result.exit_code == 0
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["daemon"] == "restarted"
    assert payload["mux_sessions"] == ["main"]
    assert payload["ok"] is True


def test_restart_no_daemon_binary_is_non_fatal(monkeypatch) -> None:
    from fno.agents import rust_runtime

    monkeypatch.setattr(rust_runtime, "resolve_installed_binary", lambda: None)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["restart"])
    assert result.exit_code == 0
    assert "no installed fno-agents binary" in result.output


def test_restart_mux_json_nothing_running_completes(monkeypatch) -> None:
    """AC (x-2896): no daemon + no mux server -> `--mux --json` completes with a
    JSON summary saying nothing was running - the 2026-07-03 hang scenario."""
    from fno.agents import rust_runtime

    monkeypatch.setattr(rust_runtime, "resolve_installed_binary", lambda: None)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [])

    result = runner.invoke(app, ["restart", "--mux", "--json"])
    assert result.exit_code == 0
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["mux_sessions"] == []
    assert payload["mux_restarted"] == []
    assert payload["ok"] is True


def test_restart_mux_kill_timeout_names_the_session(monkeypatch) -> None:
    """A kill-server that exceeds its 10s belt is reported BY NAME and fails
    the command - never a silent hang or an anonymous failure (x-2896)."""
    import subprocess as sp

    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "wedged", "state": "live"}])

    def _run(cmd, **kwargs):
        if "kill-server" in cmd:
            raise sp.TimeoutExpired(cmd, 10)
        return types.SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(restart.subprocess, "run", _run)

    result = runner.invoke(app, ["restart", "--mux"])
    assert result.exit_code == 1
    assert "gave up on mux session 'wedged'" in result.output


def test_restart_wedged_row_fails_and_names_session_and_log(monkeypatch) -> None:
    """A wedged mux row (holds the socket but not accepting) is an actionable
    failure, not a benign non-live row: `fno restart` must exit non-zero and name
    the session + its log, never report ok:true over it (x-82c6). Fires WITHOUT
    --mux -- a wedged server is broken, not a restart target you opt into."""
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(restart.subprocess, "run", _record_run([]))
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [{"session": "stuck", "state": "wedged", "log": "/tmp/mux/stuck.log"}],
    )

    result = runner.invoke(app, ["restart"])
    assert result.exit_code == 1
    assert "WEDGED" in result.output
    assert "stuck" in result.output
    assert "/tmp/mux/stuck.log" in result.output


def _revive_setup(monkeypatch, calls: list) -> None:
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(restart, "_REVIVE_SETTLE_SECS", 0)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "main", "state": "live"}])


def test_restart_mux_revives_orphaned_claude_workers(monkeypatch) -> None:
    """After --mux kills a server, a claude worker that died with it is respawned
    onto its recorded session; a worker still live after the kill (bg substrate)
    is left alone."""
    calls: list = []
    _revive_setup(monkeypatch, calls)
    orphan = {
        "name": "worker1",
        "status": "live",
        "provider": "claude",
        "session_id": "uuid-1",
        "cwd": "/w1",
    }
    survivor = {
        "name": "bgw",
        "status": "live",
        "provider": "claude",
        "session_id": "uuid-2",
        "cwd": "/w2",
    }
    _rows_seq(
        monkeypatch,
        [[orphan, survivor], [dict(orphan, status="exited"), survivor]],
    )

    result = runner.invoke(app, ["restart", "--mux", "--json"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno", "agents", "reconcile"] in calls
    assert [
        "/cargo/bin/fno", "agents", "spawn", "worker1",
        "--provider", "claude", "--substrate", "bg", "--resume", "uuid-1", "--cwd", "/w1",
    ] in calls
    assert not any("spawn" in c and "bgw" in c for c in calls), "survivor must not be respawned"
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["agents_revived"] == ["worker1"]


def test_restart_no_revive_flag_skips_revival(monkeypatch) -> None:
    calls: list = []
    _revive_setup(monkeypatch, calls)
    monkeypatch.setattr(restart, "_agents_rows", lambda: [{"name": "w", "status": "live"}])

    result = runner.invoke(app, ["restart", "--mux", "--no-revive"])
    assert result.exit_code == 0
    assert not any("spawn" in c or "reconcile" in c for c in calls)


def test_restart_revive_skips_worker_without_resumable_session(monkeypatch) -> None:
    """A dead worker with no claude session (or a non-claude provider) is
    reported as skipped, never spawned blind."""
    calls: list = []
    _revive_setup(monkeypatch, calls)
    row = {"name": "codexw", "status": "live", "provider": "codex", "session_id": None}
    _rows_seq(monkeypatch, [[row], [dict(row, status="exited")]])

    result = runner.invoke(app, ["restart", "--mux", "--json"])
    assert result.exit_code == 0
    assert not any("spawn" in c for c in calls)
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["agents_revive_skipped"] == ["codexw"]


def test_restart_revive_failure_reported_not_fatal(monkeypatch) -> None:
    """A failed revive names the worker and the manual fallback but does not fail
    the restart (which already succeeded)."""
    calls: list = []
    _revive_setup(monkeypatch, calls)

    def _run(cmd, **kwargs):
        calls.append(list(cmd))
        rc = 1 if "spawn" in cmd else 0
        return types.SimpleNamespace(returncode=rc, stdout="", stderr="")

    monkeypatch.setattr(restart.subprocess, "run", _run)
    row = {"name": "worker1", "status": "live", "provider": "claude", "session_id": "uuid-1"}
    _rows_seq(monkeypatch, [[row], [dict(row, status="exited")]])

    result = runner.invoke(app, ["restart", "--mux", "--json"])
    assert result.exit_code == 0
    assert "fno agents resume worker1" in result.output
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["agents_revive_failed"] == ["worker1"]
    assert payload["ok"] is True


def test_agents_rows_tolerates_non_dict_non_list_json(monkeypatch) -> None:
    """`fno agents list --json` returning a scalar (string/int/null) must yield []
    not an AttributeError crash (gemini medium on PR #454)."""
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda *a, **k: types.SimpleNamespace(returncode=0, stdout='"a string"', stderr=""),
    )
    assert restart._agents_rows() == []


def test_restart_wedged_row_not_killed_and_json_ok_false(monkeypatch) -> None:
    """The floor reports + fails but does NOT reap a wedged server (no kill-server
    call); the JSON summary carries it under mux_wedged with ok:false."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [
            {"session": "ok1", "state": "live"},
            {"session": "stuck", "state": "wedged", "log": "/tmp/mux/stuck.log"},
        ],
    )

    result = runner.invoke(app, ["restart", "--mux", "--json"])
    assert result.exit_code == 1
    assert not any("stuck" in c for c in calls), "must NOT kill a wedged server (floor: report only)"
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["mux_wedged"] == ["stuck"]
    assert payload["ok"] is False
