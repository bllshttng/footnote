"""Focused tests for the ``fno doctor --codex-bind`` lane canary.

The pane-binding measurement for this node found the production oracle
(rollout-fd) binding 0/20 spawns against codex 0.148, while the app-server
daemon oracle bound 10/10. `--codex-bind` is the standing canary so a future
codex upgrade that breaks either lane again reads as a red canary naming the
oracle and the installed version, not a mystery some weeks later.

Drives the exact production binding sequence (``_await_pane_binding`` +
``_make_codex_bind_probe``), so the mocking surface here mirrors
``test_spawn_pane.py``'s daemon-oracle tests rather than hand-rolling its own
poll loop.

Every test fakes the mux/subprocess seams at the module level - the suite
never spawns a real codex pane.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
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
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *a, **k: True)
    monkeypatch.setattr(mux_spawn, "_read_pane_tail", lambda *a, **k: "")
    return reaped


def test_binds_via_the_fd_oracle_and_reports_it(monkeypatch) -> None:
    reaped = _patch_common(monkeypatch)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: SID
    )
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: None)

    result = doctor._codex_bind_report()

    assert result == {
        "bound": True,
        "oracle": "rollout-fd",
        "elapsed_s": result["elapsed_s"],
        "codex_version": "codex-cli 0.148.0",
        "error": None,
    }
    assert reaped == [("main", 7)]


def test_canary_uses_isolated_trusted_cwd_and_nonempty_prompt(monkeypatch) -> None:
    _patch_common(monkeypatch)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: SID
    )
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: None)
    seen: list[str] = []

    def record_run(args, *_a, **_kw):
        seen.extend(args)
        return _proc(stdout="7\n")

    monkeypatch.setattr(mux_spawn, "_run_mux", record_run)

    result = doctor._codex_bind_report()

    assert result["bound"] is True
    cwd = Path(seen[seen.index("--cwd") + 1])
    command = seen[seen.index("--") + 1 :]
    assert cwd != Path.cwd()
    assert not cwd.exists(), "the diagnostic owns and removes its scratch directory"
    assert command[0] == "codex"
    config_index = command.index("-c")
    assert command[config_index + 1] == (
        f"projects.{json.dumps(str(cwd))}.trust_level=\"trusted\""
    )
    prompt_fence = command.index("--")
    assert command[prompt_fence + 1].strip()


def test_binding_exception_still_reaps_the_canary_pane(monkeypatch) -> None:
    reaped = _patch_common(monkeypatch)
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_binding",
        lambda *_a, **_kw: (_ for _ in ()).throw(RuntimeError("binding exploded")),
    )

    with pytest.raises(RuntimeError, match="binding exploded"):
        doctor._codex_bind_report()

    assert reaped == [("main", 7)]


def test_binds_via_the_daemon_oracle_when_the_fd_probe_misses(monkeypatch) -> None:
    _patch_common(monkeypatch)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: None
    )
    # The stability gate needs the SAME candidate on two consecutive daemon
    # probes; a constant lambda satisfies that without a call counter.
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: SID)
    monkeypatch.setattr(mux_spawn, "_CODEX_DAEMON_PROBE_INTERVAL_S", 0.0)
    monkeypatch.setattr(mux_spawn.time, "sleep", lambda *_a, **_k: None)

    result = doctor._codex_bind_report()

    assert result["bound"] is True
    assert result["oracle"] == "daemon"


def test_neither_oracle_binding_fails_named_and_reaps(monkeypatch) -> None:
    _patch_common(monkeypatch)
    monkeypatch.setattr(doctor, "_CODEX_BIND_CANARY_WINDOW_S", 0.05)
    monkeypatch.setattr(mux_spawn.time, "sleep", lambda *_a, **_k: None)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: None
    )
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: None)

    result = doctor._codex_bind_report()

    assert result["bound"] is False
    assert result["oracle"] is None
    assert "neither oracle" in result["error"]


def test_pane_run_failure_is_reported_without_a_child_pid_lookup(monkeypatch) -> None:
    _patch_common(monkeypatch, run_returncode=1, pane_id_out="boom")
    result = doctor._codex_bind_report()
    assert result["bound"] is False
    assert result["error"] == "boom"


def test_no_child_pid_names_the_pid_lookup_miss_not_a_timeout(monkeypatch) -> None:
    # A pane that ran but whose child pid never resolved never waits out a
    # window at all - the report must not claim "neither oracle bound
    # within the window", which would misattribute a lookup miss as a
    # timeout and hide the real cause.
    _patch_common(monkeypatch)
    monkeypatch.setattr(mux_spawn, "_lookup_child_pid", lambda *a, **k: None)

    result = doctor._codex_bind_report()

    assert result["bound"] is False
    assert result["oracle"] is None
    assert result["error"] == "no child pid found for the canary pane"


def test_unparseable_pane_id_on_a_successful_run_names_the_manual_cleanup_path(
    monkeypatch,
) -> None:
    # returncode 0 means the pane really was created - unlike the failure
    # case above, this must not claim "no output" or go silent about the
    # orphaned pane it cannot reap without an id.
    _patch_common(monkeypatch, run_returncode=0, pane_id_out="not-an-int\n")
    result = doctor._codex_bind_report()
    assert result["bound"] is False
    assert "unparseable pane run output" in result["error"]
    assert "fno mux pane ls --session main" in result["error"]


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


def test_codex_hooks_and_codex_bind_together_is_rejected_not_silently_dropped(
    monkeypatch,
) -> None:
    # codex_hooks is checked first; before this fix its own exclusivity
    # guard did not know about codex_bind, so the combination silently ran
    # --codex-hooks and dropped --codex-bind with no error.
    monkeypatch.setattr(
        doctor,
        "_codex_bind_report",
        lambda: (_ for _ in ()).throw(AssertionError("should never be reached")),
    )
    result = runner.invoke(app, ["doctor", "--codex-hooks", "--codex-bind"])
    assert result.exit_code == 2
    assert "--codex-hooks may only be combined with --json" in result.output


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
