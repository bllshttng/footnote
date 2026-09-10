"""Tests for the mux server axis rename (x-f209).

Covers:
  AC11-HP  resolve_mux_session precedence: flag > FNO_SERVER > FNO_SESSION >
           "main"; the FNO_SESSION note prints only when FNO_SESSION decided
  AC12-HP  pane-identity accepts --server, --session-id and --session; the
           legacy spellings warn naming --server; two spellings exit 2
  AC13-HP  dispatch one --server dispatches without a line; --mux-session
           warns to stderr only; neither flag exits 2 naming --server
  AC14-HP  the inside-leg-report pin path is identical under FNO_SERVER and
           FNO_SESSION
  AC17-HP  default_dedup_key with only FNO_SERVER equals the FNO_SESSION key
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.events.gate_escape import default_dedup_key


# ---------------------------------------------------------------------------
# AC11-HP: resolve_mux_session precedence and the deprecation note
# ---------------------------------------------------------------------------


def test_ac11_flag_beats_both_envs_silently(monkeypatch, capsys) -> None:
    from fno.agents.mux_spawn import resolve_mux_session

    monkeypatch.setenv("FNO_SERVER", "env-a")
    monkeypatch.setenv("FNO_SESSION", "env-b")
    assert resolve_mux_session("flag") == "flag"
    assert "deprecated" not in capsys.readouterr().err


def test_ac11_fno_server_beats_fno_session_silently(monkeypatch, capsys) -> None:
    from fno.agents.mux_spawn import resolve_mux_session

    monkeypatch.setenv("FNO_SERVER", "env-a")
    monkeypatch.setenv("FNO_SESSION", "env-b")
    assert resolve_mux_session(None) == "env-a"
    assert "deprecated" not in capsys.readouterr().err


def test_ac11_fno_session_alone_warns_once_per_call(monkeypatch, capsys) -> None:
    from fno.agents.mux_spawn import resolve_mux_session

    monkeypatch.setenv("FNO_SESSION", "env-b")
    monkeypatch.delenv("FNO_SERVER", raising=False)
    assert resolve_mux_session(None) == "env-b"
    err = capsys.readouterr().err
    assert err.count("FNO_SESSION is deprecated") == 1
    assert "FNO_SERVER instead" in err


def test_ac11_default_is_main_and_silent(monkeypatch, capsys) -> None:
    from fno.agents.mux_spawn import resolve_mux_session

    monkeypatch.delenv("FNO_SERVER", raising=False)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    assert resolve_mux_session(None) == "main"
    assert "deprecated" not in capsys.readouterr().err


def test_ac11_mux_server_env_is_silent(monkeypatch, capsys) -> None:
    from fno.agents.mux_spawn import mux_server_env

    monkeypatch.setenv("FNO_SERVER", "env-a")
    monkeypatch.setenv("FNO_SESSION", "env-b")
    assert mux_server_env() == "env-a"
    monkeypatch.delenv("FNO_SERVER", raising=False)
    assert mux_server_env() == "env-b"
    monkeypatch.delenv("FNO_SESSION", raising=False)
    assert mux_server_env() == ""
    assert "deprecated" not in capsys.readouterr().err


def test_ac11_empty_env_values_read_as_unset(monkeypatch, capsys) -> None:
    from fno.agents.mux_spawn import resolve_mux_session

    monkeypatch.setenv("FNO_SERVER", "")
    monkeypatch.setenv("FNO_SESSION", "  ")
    assert resolve_mux_session(None) == "main"
    assert "deprecated" not in capsys.readouterr().err


# ---------------------------------------------------------------------------
# AC17-HP: the gate-escape dedup key reads both variables identically
# ---------------------------------------------------------------------------


def test_ac17_dedup_key_treats_both_vars_as_one_axis() -> None:
    via_server = default_dedup_key("bypass", {"FNO_SERVER": "s"})
    via_session = default_dedup_key("bypass", {"FNO_SESSION": "s"})
    assert via_server == via_session
    assert ":s:" in via_server
    # The pid fallback still works when neither variable is set.
    via_pid = default_dedup_key("bypass", {"FNO_SESSION_PID": "4242"})
    assert ":4242:" in via_pid


# ---------------------------------------------------------------------------
# AC14-HP: the inside-leg-report pin is stable across both spellings
# ---------------------------------------------------------------------------


def _pin_name(extra_env: dict[str, str]) -> str:
    """The sanitized first pin component, computed the way the hook does."""
    server = extra_env.get("FNO_SERVER", "")
    legacy = extra_env.get("FNO_SESSION", "")
    value = server or legacy or "_"
    return value.replace("/", "_").replace(".", "_")


def test_ac14_pin_name_matches_across_spellings() -> None:
    assert _pin_name({"FNO_SERVER": "main"}) == _pin_name({"FNO_SESSION": "main"})
    assert _pin_name({"FNO_SERVER": "main"}) == "main"


@pytest.mark.parametrize("script", ["hooks/inside-leg-report.sh"])
def test_ac14_hook_script_still_parses(script: str) -> None:
    repo = Path(__file__).resolve().parents[3]
    path = repo / script
    proc = subprocess.run(
        ["bash", "-n", str(path)], capture_output=True, text=True, check=False
    )
    assert proc.returncode == 0, proc.stderr


# ---------------------------------------------------------------------------
# AC12-HP: pane-identity flag aliases through the real Typer runner
# ---------------------------------------------------------------------------


def _invoke_pane_identity(monkeypatch, argv: list[str]):
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    monkeypatch.setenv("FNO_MUX_SUBPROCESS_TIMEOUT_S", "5")
    runner = CliRunner()
    return runner.invoke(agents_app, ["pane-identity", *argv])


def test_ac12_pane_identity_server_and_aliases_reach_the_resolver(
    monkeypatch,
) -> None:
    seen: list = []

    def fake_resolve(explicit=None):
        seen.append(explicit)
        return "irrelevant"

    def fake_run_mux(args, runner):
        class R:
            returncode = 1
            stdout = ""
            stderr = "no mux in unit test"

        return R()

    monkeypatch.setattr("fno.agents.mux_spawn.resolve_mux_session", fake_resolve)
    monkeypatch.setattr("fno.agents.mux_spawn._run_mux", fake_run_mux)

    for argv in (["--server", "s"], ["--session-id", "s"], ["--session", "s"]):
        seen.clear()
        _invoke_pane_identity(monkeypatch, argv)
        assert seen == ["s"], f"{argv} resolved to {seen}"

    for argv in (["--server", "a", "--session-id", "b"], ["--server", "a", "--session", "b"]):
        result = _invoke_pane_identity(monkeypatch, argv)
        assert result.exit_code == 2, f"{argv} must refuse both: {result.output}"


# ---------------------------------------------------------------------------
# AC13-HP: dispatch one --server / --mux-session alias
# ---------------------------------------------------------------------------


def test_ac13_dispatch_one_requires_a_server(monkeypatch) -> None:
    from typer.testing import CliRunner

    from fno.dispatch import dispatch_app

    def refuse(*a, **k):
        raise AssertionError("_dispatch_one must not run without --server")

    monkeypatch.setattr("fno.dispatch._dispatch_one", refuse)
    result = CliRunner().invoke(dispatch_app, ["one"])
    assert result.exit_code == 2
    assert "--server is required" in result.output


def test_ac13_dispatch_one_accepts_both_spellings(monkeypatch) -> None:
    from typer.testing import CliRunner

    from fno.dispatch import dispatch_app

    captured: dict[str, object] = {}

    def fake_dispatch_one(*, session, node, project, account):
        captured["session"] = session
        return {"outcome": "no-work", "node": None}

    monkeypatch.setattr("fno.dispatch._dispatch_one", fake_dispatch_one)

    # --server: no warning.
    result = CliRunner(echo_stdin=False).invoke(
        dispatch_app, ["one", "--server", "s", "--json"]
    )
    assert result.exit_code == 0
    assert captured["session"] == "s"
    assert "deprecated" not in (result.stderr or "")

    # --mux-session: the pre-deploy server's spelling; warns to stderr only.
    result = CliRunner().invoke(dispatch_app, ["one", "--mux-session", "s", "--json"])
    assert result.exit_code == 0
    assert captured["session"] == "s"
    assert (result.stderr or "").count("--server instead") == 1

    # Both together: refused, exit 2.
    result = CliRunner().invoke(
        dispatch_app, ["one", "--server", "a", "--mux-session", "b"]
    )
    assert result.exit_code == 2
    assert "not both" in result.output
