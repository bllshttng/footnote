"""Tests for the public Python `--portal` spawn door (thread default)."""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents import cli as agents_cli
from fno.agents import mux_spawn
from fno.agents.mux_spawn import MuxSpawnResult
from fno.agents.dispatch import SpawnResult

runner = CliRunner()


def _pane_result(**kwargs) -> MuxSpawnResult:
    return MuxSpawnResult(
        name=kwargs.get("name", "w1"),
        provider="claude",
        session="mux-s",
        pane_id="%1",
        child_pid=None,
        session_uuid=None,
        short_id="abcd1234",
        status="live",
        seed="submitted",
        seed_source="delivered",
        fno_id="abcd1234",
        log_path="",
        recovered=False,
        readiness="ready",
        pane_observation="readable",
        placement=None,
        bound=True,
        pane_alive=True,
        unbound_reason=None,
    )


@pytest.fixture
def portal_probe(monkeypatch, tmp_path: Path):
    """Stub the thread dispatch and the mux-thread placement call."""
    from fno.agents import cli as cli_mod

    dispatched: dict = {}
    placed: list = []

    def fake_dispatch(**kwargs):
        dispatched.update(kwargs)
        return SpawnResult(
            kind="created", name=kwargs["name"], provider=kwargs["harness"],
            short_id="11112222", effective_message=kwargs.get("message"),
        )

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch)

    def fake_run(argv, **kwargs):
        if list(argv)[:1] == ["fno"]:
            placed.append(list(argv))
            class P:
                returncode = 0
                stderr = ""
                stdout = ""
            return P()
        import subprocess as _real_subprocess

        return _real_subprocess.run(argv, **kwargs)  # a real git call etc.

    import subprocess as _subprocess

    monkeypatch.setattr(_subprocess, "run", fake_run)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.delenv("FNO_PANE", raising=False)
    return dispatched, placed


def test_portal_opens_on_a_python_dispatched_thread_spawn(portal_probe):
    dispatched, placed = portal_probe
    result = runner.invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w1", "--harness", "claude", "--portal", "3", "hi"],
    )
    assert result.exit_code == 0, result.output
    # The substrate resolved to the built-in thread default; the portal rides
    # the two-call seam after the receipt.
    assert dispatched["headless"] is False  # the thread/bg lane
    assert placed == [["fno", "mux", "thread", "w1", "--portal", "3"]]


def test_default_spawn_inside_a_mux_opens_portal_zero(portal_probe, monkeypatch):
    dispatched, placed = portal_probe
    monkeypatch.setenv("FNO_PANE", "%7")
    result = runner.invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w1", "--harness", "claude", "hi"],
    )
    assert result.exit_code == 0, result.output
    assert placed == [["fno", "mux", "thread", "w1", "--portal", "0"]]


def test_default_spawn_outside_a_mux_stays_paneless(portal_probe):
    _dispatched, placed = portal_probe
    result = runner.invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w1", "--harness", "claude", "hi"],
    )
    assert result.exit_code == 0, result.output
    assert placed == []


def test_portal_refuses_a_range_outside_0_255(portal_probe):
    _dispatched, placed = portal_probe
    result = runner.invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w1", "--harness", "claude", "--portal", "256", "hi"],
    )
    assert result.exit_code == 2
    assert "0-255" in result.output
    assert placed == []


def test_portal_refuses_the_pane_substrate(portal_probe):
    _dispatched, placed = portal_probe
    result = runner.invoke(
        agents_cli.agents_app,
        [
            "spawn", "--name", "w1", "--harness", "claude",
            "--substrate", "pane", "--portal", "1", "hi",
        ],
    )
    assert result.exit_code == 2
    assert "thread" in result.output
    assert placed == []


def test_portal_placement_failure_keeps_the_spawn_verdict(
    portal_probe, monkeypatch, capsys
):
    """A placement failure is a partial result: the worker stays live and the
    receipt keeps its verdict; the error names the manual reach."""
    dispatched, _placed = portal_probe

    import subprocess as _subprocess

    class Boom:
        returncode = 1
        stderr = "no live mux server\n"
        stdout = ""

    monkeypatch.setattr(_subprocess, "run", lambda argv, **kw: Boom())

    result = runner.invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w1", "--harness", "claude", "--portal", "3", "hi"],
    )
    assert result.exit_code == 0, result.output
    # CliRunner merges stderr into output by default.
    assert "portal placement failed" in result.output
    assert "the worker is live" in result.output
    assert "fno mux thread w1 --portal 3" in result.output
