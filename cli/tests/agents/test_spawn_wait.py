"""`fno agents spawn --wait`: retry a REFUSING gate axis for a bounded time.

The loop lives in the CLI (the gate core has a Rust twin under a parity
harness), keyed on the receipt's `reason` field - never on refusal text,
which is how an earlier script reported SPAWNED on a provider_cap refusal.
Any other reason, a spent deadline, or `--no-wait` beside `--wait` exits at
once.

Filter: `fno doctor test cli/tests/agents/test_spawn_wait.py`
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.agents import spawn_gate
from fno.agents.spawn_gate import EXIT_LOAD_REFUSED, GateRefused
from fno.paths_testing import use_tmpdir


@pytest.fixture(autouse=True)
def _isolated(tmp_path, monkeypatch):
    for marker in (
        "FNO_SESSION",
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "FNO_SPAWN_GATE",
    ):
        monkeypatch.delenv(marker, raising=False)
    use_tmpdir(monkeypatch, tmp_path)
    yield tmp_path


def _spawn(*args: str):
    from fno.agents.cli import agents_app

    return CliRunner().invoke(agents_app, list(args), catch_exceptions=False)


def _refusing_run_gate(calls: list, reason: str):
    def _fake(
        name, substrate, *, force=False, no_wait=False, route_provider=None,
        account=None,
    ):
        calls.append(name)
        raise GateRefused(
            EXIT_LOAD_REFUSED,
            {"status": "refused", "reason": reason},
        )

    return _fake


def test_wait_retries_a_waitable_reason_until_the_deadline(monkeypatch):
    calls: list = []
    monkeypatch.setattr(spawn_gate, "run_gate", _refusing_run_gate(calls, "load_backstop"))
    result = _spawn("spawn", "-H", "claude", "--substrate", "bg", "--wait", "0.05s", "hi")
    assert result.exit_code == EXIT_LOAD_REFUSED
    assert len(calls) >= 2, "a --wait shorter than the retry sleep still retries at least once"
    assert "load_backstop" in result.stderr


def test_wait_exits_at_once_on_an_unrelated_reason(monkeypatch):
    calls: list = []
    monkeypatch.setattr(spawn_gate, "run_gate", _refusing_run_gate(calls, "king_share"))
    result = _spawn("spawn", "-H", "claude", "--substrate", "bg", "--wait", "5m", "hi")
    assert result.exit_code == EXIT_LOAD_REFUSED
    assert len(calls) == 1, "a policy refusal is not waitable; retrying it is a hang"
    assert "king_share" in result.output, "the receipt still lands for the caller"


def test_wait_with_no_wait_refuses_usage(monkeypatch):
    result = _spawn(
        "spawn", "-H", "claude", "--substrate", "bg", "--wait", "5m", "--no-wait", "hi"
    )
    assert result.exit_code == 2
    assert "mutually exclusive" in result.stderr


def test_wait_retries_the_gates_own_no_wait_refusal(monkeypatch):
    """Under --wait every attempt runs no_wait, so the gate's queueing
    refusals surface as no_wait receipts; the CLI deadline still bounds them."""
    calls: list = []
    monkeypatch.setattr(spawn_gate, "run_gate", _refusing_run_gate(calls, "no_wait"))
    result = _spawn("spawn", "-H", "claude", "--substrate", "bg", "--wait", "0.05s", "hi")
    assert result.exit_code == EXIT_LOAD_REFUSED
    assert len(calls) >= 2


def test_wait_parses_durations():
    from fno.agents.cli import _parse_wait_seconds

    assert _parse_wait_seconds("90") == 90.0
    assert _parse_wait_seconds("90s") == 90.0
    assert _parse_wait_seconds("5m") == 300.0
    assert _parse_wait_seconds("1h") == 3600.0
    with pytest.raises(ValueError):
        _parse_wait_seconds("soon")
    with pytest.raises(ValueError):
        _parse_wait_seconds("-5m")
