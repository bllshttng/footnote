"""The restart adapter's post-mux keeper refresh (x-6648).

The Rust verb owns every component leg; the Python adapter only adds the
post-kill sweep: after a PROVEN mux kill, run `restart --keepers-only
--json`, fold its receipts into the summary, and fail the verb when the
refresh cannot be proven. These tests pin the ordering and the negative
paths, including the measured failure this closes (a store keeper spawned by
the old server outliving the restart).
"""

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
import typer

import fno.restart as restart


def _summary(killed=True, ok=True):
    return {
        "daemon": "restarted",
        "store_keepers": [{"graph": "pre", "old_pid": 1, "result": "cycled"}],
        "mux": {"sessions": [{"session": "main", "killed": killed}]},
        "ok": ok,
        "verdict": "ok" if ok else "FAILED",
    }


def _main_stdout(summary):
    return "fno agents restart: keepers " + json.dumps(summary) + "\n"


def _keeper_stdout(ok=True, graph="post"):
    line = "fno agents restart: keepers " + json.dumps(
        {
            "store_keepers": [{"graph": graph, "old_pid": 7, "result": "cycled"}],
            "ok": ok,
            "verdict": "ok" if ok else "FAILED",
        }
    )
    return line + "\n"


def _install(monkeypatch, main, keeper=None):
    """Stub subprocess.run + the binary resolver; record every argv.

    `main` answers the daemon restart call; `keeper` (when given) answers the
    follow-up --keepers-only call. Every test asserts on the returned calls.
    """
    calls: list[list[str]] = []

    def _run(argv, **_kwargs):
        calls.append(list(argv))
        if argv[1:] == ["restart"]:
            return SimpleNamespace(
                returncode=main.returncode,
                stdout=main.stdout,
                stderr=main.stderr,
            )
        if argv[1:] == ["restart", "--keepers-only", "--json"]:
            assert keeper is not None, "the keeper leg must not run in this test"
            return SimpleNamespace(
                returncode=0,
                stdout=keeper.stdout,
                stderr=keeper.stderr,
            )
        raise AssertionError(f"unexpected subprocess: {argv}")

    monkeypatch.setattr(restart.subprocess, "run", _run)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary",
        lambda: Path("/fake/fno-agents"),
    )
    return calls


def _run_adapter(mux, json_out):
    with pytest.raises(typer.Exit) as excinfo:
        restart.restart_command(force=False, mux=mux, json_out=json_out)
    return excinfo.value.exit_code


def test_keeper_leg_runs_after_a_proven_mux_kill(monkeypatch):
    """AC4-HP: kill proven -> the keeper-only subprocess runs after the main
    restart, and its receipts fold into the summary."""
    main = SimpleNamespace(
        returncode=0, stdout=_main_stdout(_summary(killed=True)), stderr=""
    )
    keeper = SimpleNamespace(
        returncode=0, stdout=_keeper_stdout(ok=True), stderr="keeper receipt\n"
    )
    calls = _install(monkeypatch, main, keeper)

    code = _run_adapter(mux=True, json_out=True)

    assert code == 0
    assert len(calls) == 2, "main restart then the keeper-only leg"
    assert calls[1][1:] == ["restart", "--keepers-only", "--json"]


def test_folded_summary_names_the_post_mux_keeper(monkeypatch, capsys):
    main = SimpleNamespace(
        returncode=0, stdout=_main_stdout(_summary(killed=True)), stderr=""
    )
    keeper = SimpleNamespace(
        returncode=0, stdout=_keeper_stdout(ok=True, graph="late-keeper"), stderr=""
    )
    _install(monkeypatch, main, keeper)

    code = _run_adapter(mux=True, json_out=True)

    out = capsys.readouterr().out
    assert code == 0
    folded = restart._keepers_summary(out)
    assert folded is not None, "the adapter reprints ONE machine line"
    assert folded["post_mux_keeper_refresh"] == "proved"
    assert folded["post_mux_store_keepers"] == [
        {"graph": "late-keeper", "old_pid": 7, "result": "cycled"}
    ]
    assert folded["ok"] is True


def test_no_kill_launches_no_keeper_subprocess(monkeypatch):
    """AC4-ERR: no killed session -> no second subprocess, exit passes through."""
    main = SimpleNamespace(
        returncode=0, stdout=_main_stdout(_summary(killed=False)), stderr=""
    )
    calls = _install(monkeypatch, main)

    code = _run_adapter(mux=True, json_out=True)

    assert code == 0
    assert len(calls) == 1, "only the main restart ran"


def test_daemon_only_restart_skips_the_leg(monkeypatch):
    main = SimpleNamespace(returncode=0, stdout=_main_stdout(_summary(killed=True)), stderr="")
    calls = _install(monkeypatch, main)

    code = _run_adapter(mux=False, json_out=True)

    assert code == 0
    assert len(calls) == 1, "daemon-only behavior is unchanged"


def test_unparsable_summary_skips_the_leg(monkeypatch):
    """No keepers line -> no PROVEN kill -> the leg does not run."""
    main = SimpleNamespace(returncode=0, stdout="human receipts only\n", stderr="")
    calls = _install(monkeypatch, main)

    code = _run_adapter(mux=True, json_out=True)

    assert code == 0
    assert len(calls) == 1


def test_unproven_keeper_refresh_fails_the_verb(monkeypatch, capsys):
    """AC4-ERR: a spared keeper (ok=false) fails the overall command."""
    main = SimpleNamespace(
        returncode=0, stdout=_main_stdout(_summary(killed=True)), stderr=""
    )
    keeper = SimpleNamespace(
        returncode=1, stdout=_keeper_stdout(ok=False), stderr="spared receipt\n"
    )
    _install(monkeypatch, main, keeper)

    code = _run_adapter(mux=True, json_out=True)

    err = capsys.readouterr().err
    assert code == 1
    assert "unproven" in err


def test_folded_summary_carries_failed_when_refresh_unproven(monkeypatch, capsys):
    main = SimpleNamespace(
        returncode=0, stdout=_main_stdout(_summary(killed=True)), stderr=""
    )
    keeper = SimpleNamespace(
        returncode=1, stdout=_keeper_stdout(ok=False), stderr=""
    )
    _install(monkeypatch, main, keeper)

    _run_adapter(mux=True, json_out=True)

    folded = restart._keepers_summary(capsys.readouterr().out)
    assert folded is not None
    assert folded["ok"] is False
    assert folded["verdict"] == "FAILED"
    assert folded["post_mux_keeper_refresh"] == "unproven"


def test_failed_main_restart_skips_the_leg(monkeypatch):
    main = SimpleNamespace(
        returncode=1, stdout=_main_stdout(_summary(killed=True)), stderr="boom\n"
    )
    calls = _install(monkeypatch, main)

    code = _run_adapter(mux=True, json_out=True)

    assert code == 1
    assert len(calls) == 1


def test_keepers_summary_parses_last_line_only():
    lines = [
        "fno agents restart: keepers " + json.dumps({"ok": True}) + "\n",
        "fno agents restart: keepers " + json.dumps({"ok": False}) + "\n",
    ]
    assert restart._keepers_summary("".join(lines))["ok"] is False
    assert restart._keepers_summary("") is None
    assert restart._keepers_summary("no machine line\n") is None
    assert (
        restart._keepers_summary("fno agents restart: keepers {broken\n") is None
    )


def test_any_mux_killed_needs_a_proven_true():
    assert restart._any_mux_killed(None) is False
    assert restart._any_mux_killed({}) is False
    assert restart._any_mux_killed({"mux": {}}) is False
    assert restart._any_mux_killed({"mux": {"sessions": [{"killed": False}]}}) is False
    assert restart._any_mux_killed({"mux": {"sessions": [{"killed": True}]}}) is True
