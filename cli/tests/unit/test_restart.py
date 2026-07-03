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
        return types.SimpleNamespace(returncode=0)

    return _run


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
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "main"}, {"session": "work"}])

    result = runner.invoke(app, ["restart", "--mux"])
    assert result.exit_code == 0
    assert ["/cargo/bin/fno", "mux", "kill-server", "main"] in calls
    assert ["/cargo/bin/fno", "mux", "kill-server", "work"] in calls


def test_restart_json_summary(monkeypatch) -> None:
    _fake_daemon_binary(monkeypatch)
    monkeypatch.setattr(
        restart.subprocess, "run", lambda cmd, **k: types.SimpleNamespace(returncode=0)
    )
    monkeypatch.setattr(restart, "_mux_sessions", lambda: [{"session": "main"}])

    result = runner.invoke(app, ["restart", "--json"])
    assert result.exit_code == 0
    payload = json.loads([ln for ln in result.output.splitlines() if ln.strip().startswith("{")][-1])
    assert payload["daemon"] == "restarted"
    assert payload["mux_sessions"] == ["main"]


def test_restart_no_daemon_binary_is_non_fatal(monkeypatch) -> None:
    from fno.agents import rust_runtime

    monkeypatch.setattr(rust_runtime, "resolve_installed_binary", lambda: None)
    monkeypatch.setattr(restart, "_mux_sessions", lambda: None)

    result = runner.invoke(app, ["restart"])
    assert result.exit_code == 0
    assert "no installed fno-agents binary" in result.output
