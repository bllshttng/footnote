"""Unit tests for `fno agents restart` (x-69b3)."""
from __future__ import annotations

import json
import types
from pathlib import Path

from typer.testing import CliRunner

from fno import restart
from fno.cli import app

runner = CliRunner()


def _fake_daemon_binary(monkeypatch, path: str = "/cargo/bin/fno-agents") -> None:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_installed_binary", lambda: Path(path))


_REAL_RUN = __import__("subprocess").run


def _record_run(calls: list) -> object:
    def _run(cmd, **kwargs):
        calls.append(list(cmd))
        # The name verbs execute in the binary; a blanket empty stub would
        # answer the mint with an empty stdout.
        parts = [str(part) for part in cmd]
        if "name-mint" in parts or "name-codes" in parts or "name-parse" in parts:
            return _REAL_RUN(cmd, **kwargs)
        return types.SimpleNamespace(returncode=0, stdout="", stderr="")

    return _run


def test_restart_restarts_daemon_and_reports_mux(monkeypatch) -> None:
    """Default: restart the daemon, REPORT (not kill) live mux sessions."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "main", "state": "live"}])

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno-agents", "restart"] in calls
    assert not any("kill-server" in c for c in calls), "must NOT kill mux without --mux"
    assert "live mux session" in result.output


def test_agents_restart_force_preserves_the_daemon_break_glass_flag(monkeypatch) -> None:
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart", "--force"])

    assert result.exit_code == 0, result.output
    assert ["/cargo/bin/fno-agents", "restart", "--force"] in calls


def test_daemon_refusal_renders_once_in_the_command_voice(monkeypatch) -> None:
    """A binary refusal prints ONCE, prefixed by this command."""
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda cmd, **kw: types.SimpleNamespace(
            returncode=2,
            stdout="",
            stderr="fno-agents: restart takes no arguments besides --force (got: --json)\n",
        ),
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])

    assert result.exit_code == 1
    assert "fno agents restart: fno-agents restart exited 2" in result.output
    assert result.output.count("takes no arguments besides --force") == 1


def test_daemon_stderr_note_is_relayed_once_on_success(monkeypatch) -> None:
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda cmd, **kw: types.SimpleNamespace(
            returncode=0,
            stdout="restarted: pid 100 -> 200\n",
            stderr="note: declining recycled pid\n",
        ),
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])

    assert result.exit_code == 0, result.output
    assert "agents daemon restarted" in result.output
    assert result.output.count("note: declining recycled pid") == 1


def test_restart_mux_flag_kills_each_session(monkeypatch) -> None:
    """--mux: kill each live mux session so it respawns on the new binary."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [
            {"session": "main", "state": "live", "stale": True, "panes": 2},
            {"session": "work", "state": "live", "stale": False, "panes": 1},
        ],
    )

    result = runner.invoke(app, ["agents", "restart", "--mux"])
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

    result = runner.invoke(app, ["agents", "restart", "--mux"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno", "mux", "kill-server", "live1"] in calls
    assert not any("dead" in c for c in calls), "must NOT kill a non-live session"


def test_restart_spares_stale_wire_server_with_live_panes(monkeypatch) -> None:
    """A stale-wire server with live panes is spared, but sparing is a
    reported failure, not success: exit-code automation must see the fleet is
    still skewed. A current-wire server stays opt-in as well."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [
            {"session": "old", "state": "live", "stale": True, "panes": 2},
            {"session": "cur", "state": "live", "stale": False, "panes": 0},
        ],
    )

    result = runner.invoke(app, ["agents", "restart", "--json"])
    assert result.exit_code == 1, "a spared stale-wire server is an unrestored fleet"
    assert not any(
        "kill-server" in c for c in calls
    ), "live-pane and current-wire servers are left opt-in"
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["mux_spared"] == ["old"]
    assert payload["ok"] is False
    assert "spared" in result.output and "--mux" in result.output, "names the break-glass lever"
    assert "current wire" in result.output, "current-wire server reported, not killed"


def test_restart_auto_restarts_stale_wire_server_without_live_panes(monkeypatch) -> None:
    """A stale-wire server with no live panes still heals pair-deploy skew."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    monkeypatch.setattr(restart.subprocess, "run", _record_run(calls))
    monkeypatch.setattr(restart.shutil, "which", lambda n: "/cargo/bin/fno")
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [{"session": "old", "state": "live", "stale": True, "panes": 0}],
    )

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno", "mux", "kill-server", "old"] in calls


def test_restart_daemon_failure_exits_nonzero(monkeypatch) -> None:
    """A real daemon-restart failure fails the command (scripts must see it)."""
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda cmd, **k: types.SimpleNamespace(returncode=3, stdout="", stderr="daemon boom"),
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 1
    assert "exited 3" in result.output


def test_restart_daemon_failure_renders_stderr_once(monkeypatch) -> None:
    """The adapter's one line carries the failure detail; the raw daemon
    stderr is never double-echoed (x-67b8: the duplicate echo claimed the
    internal --json came from the operator)."""
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda cmd, **k: types.SimpleNamespace(
            returncode=2, stdout="", stderr="fno-agents restart: unexpected argument '--bogus'"
        ),
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 1
    assert "exited 2" in result.output
    # The refusal detail reaches the operator only inside the adapter's own
    # prefixed lines (the failure line and the FAILED verdict); the raw
    # daemon stderr is never echoed as a standalone line (x-67b8: the old
    # echo claimed the internal --json came from the operator).
    offenders = [
        line
        for line in result.output.splitlines()
        if "unexpected argument" in line and not line.startswith("fno agents restart:")
    ]
    assert offenders == []


def test_restart_json_summary(monkeypatch) -> None:
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda cmd, **k: types.SimpleNamespace(returncode=0, stdout="", stderr=""),
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "main", "state": "live"}])

    result = runner.invoke(app, ["agents", "restart", "--json"])
    assert result.exit_code == 0
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["daemon"] == "restarted"
    assert payload["mux_sessions"] == ["main"]
    assert payload["ok"] is True


def test_restart_no_daemon_binary_is_non_fatal(monkeypatch) -> None:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_installed_binary", lambda: None)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 0
    assert "no installed fno-agents binary" in result.output


def test_restart_mux_json_nothing_running_completes(monkeypatch) -> None:
    """AC (x-2896): no daemon + no mux server -> `--mux --json` completes with a
    JSON summary saying nothing was running - the 2026-07-03 hang scenario."""
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_installed_binary", lambda: None)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [])

    result = runner.invoke(app, ["agents", "restart", "--mux", "--json"])
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

    result = runner.invoke(app, ["agents", "restart", "--mux"])
    assert result.exit_code == 1
    assert "gave up on mux session 'wedged'" in result.output


def test_restart_wedged_row_fails_and_names_session_and_log(monkeypatch) -> None:
    """A wedged mux row (holds the socket but not accepting) is an actionable
    failure, not a benign non-live row: `fno agents restart` must exit non-zero and name
    the session + its log, never report ok:true over it (x-82c6). Fires WITHOUT
    --mux -- a wedged server is broken, not a restart target you opt into."""
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(restart.subprocess, "run", _record_run([]))
    monkeypatch.setattr(
        restart,
        "_mux_sessions",
        lambda: [{"session": "stuck", "state": "wedged", "log": "/tmp/mux/stuck.log"}],
    )

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 1
    assert "WEDGED" in result.output
    assert "stuck" in result.output
    assert "/tmp/mux/stuck.log" in result.output


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

    result = runner.invoke(app, ["agents", "restart", "--mux", "--json"])
    assert result.exit_code == 1
    assert not any("stuck" in c for c in calls), "must NOT kill a wedged server (floor: report only)"
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["mux_wedged"] == ["stuck"]
    assert payload["ok"] is False


def _quiet_keeper_leg(monkeypatch) -> None:
    """A daemon restart with no keeper summary line: the keeper leg is absent."""
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda cmd, **k: types.SimpleNamespace(returncode=0, stdout="restarted: pid 1 -> 2", stderr=""),
    )


def test_restart_cycles_stale_store_keeper_and_ends_on_verdict(monkeypatch) -> None:
    """AC6-HP: stale store keeper cycled, no mux killed, ok verdict last."""
    _fake_daemon_binary(monkeypatch)
    calls: list = []
    keeper_json = json.dumps(
        {
            "store_keepers": [
                {"graph": "/tmp/graph.json", "old_pid": 11, "result": "cycled"}
            ],
            "pane_keepers_stale": 1,
        }
    )

    def _run(cmd, **kwargs):
        calls.append(list(cmd))
        return types.SimpleNamespace(
            returncode=0,
            stdout=(
                "restarted: pid 1 -> 2\n"
                "fno agents restart: store keeper /tmp/graph.json pid 11 shut down "
                "(stale build; respawns on next read).\n"
                "fno agents restart: 1 pane keeper(s) run an older build; kept with "
                "their panes, current when each pane ends.\n"
                f"fno agents restart: keepers {keeper_json}"
            ),
            stderr="",
        )

    monkeypatch.setattr(restart.subprocess, "run", _run)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 0, result.output
    assert "restarted: pid 1 -> 2" in result.output, "the daemon's own receipt survives"
    assert "store keeper /tmp/graph.json pid 11 shut down" in result.output, result.output
    assert "1 pane keeper(s) run an older build" in result.output
    assert "kept with their panes" in result.output
    assert ["restart"] == calls[0][-1:], "the daemon leg passes no subcommand flags"
    assert not any("kill-server" in c for c in calls), "no mux kill"
    last = [ln for ln in result.output.splitlines() if ln.strip()][-1]
    assert last.startswith("fno agents restart: ok - "), last


def test_restart_verdict_is_failed_when_daemon_fails(monkeypatch) -> None:
    """AC6-ERR: a failed daemon ends on a FAILED verdict naming the error, exit 1."""
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess,
        "run",
        lambda cmd, **k: types.SimpleNamespace(
            returncode=1, stdout="", stderr="daemon pid 5 survived SIGKILL"
        ),
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 1
    lines = [ln for ln in result.output.splitlines() if ln.strip()]
    assert lines[-1].startswith("fno agents restart: FAILED - "), lines[-1]
    assert result.output.count("survived SIGKILL") == 1, "the error text appears once"
    assert "survived SIGKILL" not in lines[-1], "the verdict names the leg; the say line carries the text"


def test_restart_spared_store_keeper_fails_the_verb(monkeypatch) -> None:
    """AC6-EDGE: a busy-spared keeper is named and the verb exits 1."""
    _fake_daemon_binary(monkeypatch)
    keeper_json = json.dumps(
        {
            "store_keepers": [
                {
                    "graph": "/tmp/graph.json",
                    "old_pid": 11,
                    "result": "spared: a mutation is in flight",
                }
            ],
            "pane_keepers_stale": 0,
        }
    )

    def _run(cmd, **kwargs):
        return types.SimpleNamespace(
            returncode=1,
            stdout=f"restarted: pid 1 -> 2\nfno agents restart: keepers {keeper_json}",
            stderr="",
        )

    monkeypatch.setattr(restart.subprocess, "run", _run)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["agents", "restart"])
    assert result.exit_code == 1, result.output
    assert "spared: a mutation is in flight" in result.output
    lines = [ln for ln in result.output.splitlines() if ln.strip()]
    assert lines[-1].startswith("fno agents restart: FAILED - "), lines[-1]


