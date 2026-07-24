"""CLI wiring for `fno agents spawn --account <id>` (x-d012, US2).

The four-lane overlay resolution is unit-tested in
`src/fno/agents/test_account_env.py`; this pins the CLI wiring: a resolved
overlay reaches dispatch_spawn_pane as `account_env` and the receipt names the
account (AC1-HP), and a resolver refusal fails closed before any spawn
(AC1-ERR / AC2-ERR).
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
    for m in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)


def _stub_pane_path(monkeypatch) -> dict:
    received: dict = {}
    from fno.agents import mux_spawn, spawn_gate

    class _Gate:
        def release(self) -> None:
            pass

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda *a, **k: None)

    def fake_pane(**kwargs):
        received.update(kwargs)
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"], provider=kwargs["provider"], session="s",
            pane_id=1, child_pid=None, session_uuid=None,
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_pane)
    return received


def test_account_overlay_threads_to_pane_and_receipt(monkeypatch, runner):
    """AC1-HP: a resolved overlay reaches dispatch_spawn_pane; receipt names it."""
    received = _stub_pane_path(monkeypatch)
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    # cmd_spawn calls resolve_account_overlay_or_exit, which wraps
    # resolve_account_overlay; patching the inner call is enough.
    monkeypatch.setattr(
        ae, "resolve_account_overlay",
        lambda *a, **k: AccountOverlay(
            "readyrule", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"
        ),
    )

    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "--name", "w1", "hi", "--account", "readyrule", "--here"]
    )
    assert result.exit_code == 0, result.output
    assert received["account_env"] == {"CLAUDE_CONFIG_DIR": "/x/.claude"}
    receipt = json.loads(
        next(ln for ln in result.output.splitlines() if ln.startswith("{"))
    )
    assert receipt["account"] == "readyrule"


def test_account_refusal_fails_closed(monkeypatch, runner):
    """AC1-ERR: a resolver refusal exits 2 before any spawn (pane stub untouched)."""
    received = _stub_pane_path(monkeypatch)
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountResolutionError

    def boom(*a, **k):
        raise AccountResolutionError("account 'nope' is not a registered provider.")

    monkeypatch.setattr(ae, "resolve_account_overlay", boom)

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "--name", "w1", "hi", "--account", "nope"])
    assert result.exit_code == 2
    assert "not a registered provider" in result.output
    assert received == {}  # never reached the pane dispatch


def test_account_non_claude_provider_refused(monkeypatch, runner):
    """--account with a non-claude provider is a user error caught before spawn."""
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "--name", "w1", "hi", "--harness", "codex", "--account", "x"]
    )
    assert result.exit_code == 2
    assert "claude-only" in result.output
    assert received == {}


def test_account_plus_route_refused(monkeypatch, runner):
    """--account + --route is contradictory (bill claude account vs route away)."""
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--account", "readyrule", "--substrate", "bg",
         "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 2
    assert "cannot combine with --route" in result.output
    assert received == {}


def test_account_plus_role_refused(monkeypatch, runner):
    """--account + --role is refused (role auto-routing would mis-bill)."""
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "--name", "w1", "hi", "--account", "readyrule", "--role", "tidy"]
    )
    assert result.exit_code == 2
    assert "cannot combine with --role" in result.output
    assert received == {}
