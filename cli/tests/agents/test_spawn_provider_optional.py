"""End-to-end Typer wiring for the optional ``--provider`` on ``fno agents spawn``.

The resolver's precedence/validation is unit-tested in
``agents/test_provider_resolve.py``; this pins the CLI wiring: an omitted
``--provider`` defaults through the resolver to the pane receipt (AC2-HP), and an
empty ``--model`` is rejected before anything spawns (AC2-ERR).
"""

from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _no_harness_markers(monkeypatch):
    for m in (
        "CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"
    ):
        monkeypatch.delenv(m, raising=False)


def _stub_pane_path(monkeypatch) -> dict:
    """Stub the spawn gate + pane dispatch so cmd_spawn's default (pane) path
    runs without a real mux; return the kwargs dispatch_spawn_pane received."""
    received: dict = {}

    from fno.agents import mux_spawn, spawn_gate

    class _Gate:
        def release(self) -> None:  # noqa: D401 - test double
            pass

    def fake_run_gate(*a, **k):
        return _Gate()

    def fake_resolve_provenance(*a, **k):
        return None

    def fake_dispatch_spawn_pane(**kwargs):
        received.update(kwargs)
        # Echo the resolved provider back so the receipt reflects resolution.
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="sess-1",
            pane_id=1,
            child_pid=None,
            session_uuid=None,
        )

    monkeypatch.setattr(spawn_gate, "run_gate", fake_run_gate)
    monkeypatch.setattr(mux_spawn, "resolve_provenance", fake_resolve_provenance)
    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch_spawn_pane)
    return received


def test_spawn_without_provider_defaults_to_claude(monkeypatch, runner):
    """AC2-HP/AC1-EDGE: no --provider -> resolved claude reaches the pane receipt."""
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "w1", "hello", "--node", "x-test"])

    assert result.exit_code == 0, result.output
    assert received["provider"] == "claude"
    receipt = json.loads(result.stdout.strip().splitlines()[-1])
    assert receipt["provider"] == "claude"
    assert receipt["provider_source"] == "builtin-default"


def test_spawn_infers_claude_from_harness(monkeypatch, runner):
    """AC2-HP: inside a claude-code session, no -p infers provider=claude."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sid-abc")
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "w1", "hello", "--node", "x-test"])

    assert result.exit_code == 0, result.output
    assert received["provider"] == "claude"
    receipt = json.loads(result.stdout.strip().splitlines()[-1])
    assert receipt["provider_source"] == "harness-inferred"


def test_spawn_explicit_provider_still_wins(monkeypatch, runner):
    monkeypatch.setenv("CODEX_SESSION_ID", "sid-x")  # ambient harness = codex
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "w1", "hi", "--node", "x-test", "--provider", "gemini"]
    )
    assert result.exit_code == 0, result.output
    assert received["provider"] == "gemini"


def test_spawn_empty_model_rejected(monkeypatch, runner):
    """AC2-ERR: --model '' exits 2 before any spawn."""
    called = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "w1", "hi", "--node", "x-test", "--model", ""]
    )
    assert result.exit_code == 2
    assert called == {}
