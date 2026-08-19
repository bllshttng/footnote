"""Focused tests for the ``fno doctor --codex-bind`` lane canary (x-e336).

The pane-binding measurement for this node found the production oracle
(rollout-fd) binding 0/20 spawns against codex 0.148, while the app-server
daemon oracle bound 10/10. `--codex-bind` is the standing canary so a future
codex upgrade that breaks either lane again reads as a red canary naming the
oracle and the installed version, not a mystery some weeks later.

Every test fakes the mux/subprocess seams at the module level - the suite
never spawns a real codex pane.
"""
from __future__ import annotations

import subprocess
import time

from typer.testing import CliRunner

from fno import doctor
from fno.agents import mux_spawn
from fno.cli import app

runner = CliRunner()
SID = "019cc081-de0d-7283-97cc-751c46742a07"


def _proc(returncode: int = 0, stdout: str = "") -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr="")


def _patch_common(monkeypatch, *, run_returncode: int = 0, pane_id_out: str = "7\n"):
    monkeypatch.setattr(doctor, "_codex_version", lambda: "codex-cli 0.148.0")
    monkeypatch.setattr(mux_spawn, "resolve_mux_session", lambda: "main")
    monkeypatch.setattr(
        mux_spawn, "_run_mux", lambda *a, **k: _proc(run_returncode, pane_id_out)
    )
    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *a, **k: 4242)
    reaped: list[tuple] = []
    monkeypatch.setattr(
        mux_spawn,
        "_reap_spawned_pane",
        lambda session, pane_id, runner: (reaped.append((session, pane_id)), (True, ""))[1],
    )
    monkeypatch.setattr(mux_spawn, "_codex_session_ids_loaded", lambda cwd: set())
    return reaped


def test_binds_via_the_fd_oracle_and_reports_it(monkeypatch) -> None:
    reaped = _patch_common(monkeypatch)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: SID
    )
    monkeypatch.setattr(mux_spawn, "_codex_daemon_bind", lambda *a, **k: None)

    result = doctor._codex_bind_report()

    assert result == {
        "bound": True,
        "oracle": "rollout-fd",
        "elapsed_s": result["elapsed_s"],
        "codex_version": "codex-cli 0.148.0",
        "error": None,
    }
    assert reaped == [("main", 7)]


def test_binds_via_the_daemon_oracle_when_the_fd_probe_misses(monkeypatch) -> None:
    _patch_common(monkeypatch)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: None
    )
    monkeypatch.setattr(mux_spawn, "_codex_daemon_bind", lambda *a, **k: SID)

    result = doctor._codex_bind_report()

    assert result["bound"] is True
    assert result["oracle"] == "daemon"


def test_neither_oracle_binding_fails_named_and_reaps(monkeypatch) -> None:
    _patch_common(monkeypatch)
    monkeypatch.setattr(doctor, "_CODEX_BIND_CANARY_WINDOW_S", 0.05)
    monkeypatch.setattr(time, "sleep", lambda *_a, **_k: None)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: None
    )
    monkeypatch.setattr(mux_spawn, "_codex_daemon_bind", lambda *a, **k: None)

    result = doctor._codex_bind_report()

    assert result["bound"] is False
    assert result["oracle"] is None
    assert "neither oracle" in result["error"]


def test_pane_run_failure_is_reported_without_a_child_pid_lookup(monkeypatch) -> None:
    _patch_common(monkeypatch, run_returncode=1, pane_id_out="boom")
    result = doctor._codex_bind_report()
    assert result["bound"] is False
    assert result["error"] == "boom"


def test_cli_exits_nonzero_on_a_failed_bind(monkeypatch) -> None:
    monkeypatch.setattr(
        doctor,
        "_codex_bind_report",
        lambda: {
            "bound": False, "oracle": None, "elapsed_s": 0.1,
            "codex_version": "codex-cli 0.148.0", "error": "neither oracle bound",
        },
    )
    result = runner.invoke(app, ["doctor", "--codex-bind"])
    assert result.exit_code == 1
    assert "FAILED" in result.output


def test_cli_exits_zero_on_a_bound_pane(monkeypatch) -> None:
    monkeypatch.setattr(
        doctor,
        "_codex_bind_report",
        lambda: {
            "bound": True, "oracle": "daemon", "elapsed_s": 3.71,
            "codex_version": "codex-cli 0.148.0", "error": None,
        },
    )
    result = runner.invoke(app, ["doctor", "--codex-bind"])
    assert result.exit_code == 0
    assert "oracle=daemon" in result.output
