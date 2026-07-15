"""cmd_spawn gate wiring (x-c5cc task 2.1): the gate runs BEFORE the substrate
fan-out, with the effective substrate, and the receipt stays byte-identical.
"""
from __future__ import annotations

import json
import os

import pytest
from typer.testing import CliRunner

from fno.agents import spawn_gate


@pytest.fixture(autouse=True)
def _canonical_is_caller(monkeypatch):
    # x-85fe: pin canonical == caller so a node-less spawn does NOT move to the
    # canonical root (the AC1-EDGE no-op). These tests exercise gate wiring and
    # receipt shape, not the cwd move; without this, a run from a linked worktree
    # resolves canonical to the main checkout and the receipt/redirect note drift.
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def gate_calls(monkeypatch):
    """Record run_gate invocations; return a releasable guard."""
    calls: list[dict] = []

    class FakeGuard:
        released = 0

        def release(self):
            FakeGuard.released += 1

    def fake_run_gate(name, substrate, *, force=False, no_wait=False):
        calls.append(
            {"name": name, "substrate": substrate, "force": force, "no_wait": no_wait}
        )
        return FakeGuard()

    monkeypatch.setattr(spawn_gate, "run_gate", fake_run_gate)
    calls_guard = (calls, FakeGuard)
    return calls_guard


def _fake_created(monkeypatch):
    """Stub dispatch_spawn to a created claude bg result."""
    from fno.agents import dispatch as dispatch_mod

    class R:
        kind = "created"
        name = "w1"
        short_id = "abcd1234"
        provider = "claude"
        reply = None

    monkeypatch.setattr(dispatch_mod, "dispatch_spawn", lambda **kw: R())


def test_bg_spawn_gates_as_bg_and_receipt_is_byte_identical(
    runner, gate_calls, monkeypatch
):
    """AC1-HP: under-cap spawn dispatches; stdout receipt shape unchanged."""
    calls, FakeGuard = gate_calls
    _fake_created(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg"],
    )
    assert result.exit_code == 0, result.output
    assert calls == [
        {"name": "w1", "substrate": "bg", "force": False, "no_wait": False}
    ]
    # Hand-rolled f-string receipt, byte-parity with the Rust path (LD10).
    line = result.stdout.strip().splitlines()[-1]
    assert (
        line
        == '{"name": "w1", "short_id": "abcd1234", "provider": "claude", "status": "live"}'
    )
    assert FakeGuard.released >= 1


def test_once_maps_to_headless_for_the_gate(runner, gate_calls, monkeypatch):
    calls, _ = gate_calls
    from fno.agents import dispatch as dispatch_mod

    class R:
        kind = "reply"
        reply = "done"

    monkeypatch.setattr(dispatch_mod, "dispatch_spawn", lambda **kw: R())
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "w1", "hi", "--provider", "codex", "--once"]
    )
    assert result.exit_code == 0, result.output
    assert calls[0]["substrate"] == "headless"


def test_gate_refusal_propagates_exit_code(runner, monkeypatch):
    """A gate refusal exits with the gate's code; nothing dispatches."""

    def refuse(*a, **k):
        raise spawn_gate.GateRefused(spawn_gate.EXIT_NO_WAIT)

    monkeypatch.setattr(spawn_gate, "run_gate", refuse)
    dispatched = []
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod, "dispatch_spawn", lambda **kw: dispatched.append(1)
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        [
            "spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg",
            "--no-wait",
        ],
    )
    assert result.exit_code == spawn_gate.EXIT_NO_WAIT
    assert not dispatched


def test_force_and_no_wait_flags_reach_the_gate(runner, gate_calls, monkeypatch):
    calls, _ = gate_calls
    _fake_created(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        [
            "spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg",
            "--force", "--no-wait",
        ],
    )
    assert result.exit_code == 0, result.output
    assert calls[0]["force"] is True
    assert calls[0]["no_wait"] is True


def test_pane_spawn_gates_as_pane_and_releases_on_success(
    runner, gate_calls, monkeypatch
):
    calls, FakeGuard = gate_calls
    from fno.agents import mux_spawn as mux_mod

    class PaneResult:
        name = "w1"
        provider = "claude"
        session = "mux-s"
        pane_id = "%1"

    monkeypatch.setattr(
        mux_mod, "dispatch_spawn_pane", lambda **kw: PaneResult()
    )
    monkeypatch.setattr(mux_mod, "resolve_provenance", lambda *a: None)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "w1", "hi", "--provider", "claude"]
    )
    assert result.exit_code == 0, result.output
    assert calls[0]["substrate"] == "pane"
    receipt = json.loads(result.stdout.strip().splitlines()[-1])
    assert receipt["mux_session"] == "mux-s"
    assert FakeGuard.released >= 1
