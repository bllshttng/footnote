"""Typer CliRunner tests for the US4-lifecycle CLI verbs.

Covers ``fno agents stop``, ``fno agents rm``, ``fno agents reconcile``,
and ``fno agents attach``. Dispatch is monkeypatched per-test so the
CLI layer is exercised in isolation; the dispatch layer has its own
test in ``test_dispatch_lifecycle.py``.

Also pins the ``ping`` informational-stub conversion from US4-lifecycle
task 2.2 (the last ``_NOT_IMPLEMENTED`` placeholder is gone).
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import pytest
from typer.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


# ---------------------------------------------------------------------------
# stop
# ---------------------------------------------------------------------------


def test_cli_stop_happy_path(monkeypatch, runner: CliRunner) -> None:
    """cmd_stop wires to dispatch.stop_agent and returns exit 0 on success."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    called: list[str] = []

    def fake_stop_agent(name: str):
        called.append(name)
        return dispatch.StopResult(name=name, provider="claude", claude_exit=0)

    monkeypatch.setattr(dispatch, "stop_agent", fake_stop_agent)
    monkeypatch.setattr(
        "fno.agents.cli.__import__",
        __builtins__["__import__"],
        raising=False,
    )

    result = runner.invoke(agents_app, ["stop", "worker-claude"])
    assert result.exit_code == 0, result.output
    assert called == ["worker-claude"]


def test_cli_stop_propagates_dispatch_exit_code(
    monkeypatch, runner: CliRunner
) -> None:
    """A DispatchAskError raised by stop_agent surfaces as Typer's exit code."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    def fake_stop_agent(name: str):
        raise dispatch.DispatchAskError(
            f"agent {name!r} not found in registry", exit_code=2
        )

    monkeypatch.setattr(dispatch, "stop_agent", fake_stop_agent)

    result = runner.invoke(agents_app, ["stop", "ghost"])
    assert result.exit_code == 2
    assert "not found in registry" in result.output


# ---------------------------------------------------------------------------
# rm
# ---------------------------------------------------------------------------


def test_cli_rm_passes_force_flag(monkeypatch, runner: CliRunner) -> None:
    """--force is wired through to dispatch.rm_agent."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    received: dict = {}

    def fake_rm_agent(name: str, *, force: bool = False):
        received["name"] = name
        received["force"] = force
        return dispatch.RmResult(
            name=name,
            provider="claude",
            claude_exit=1,
            force=force,
            registry_changed=True,
        )

    monkeypatch.setattr(dispatch, "rm_agent", fake_rm_agent)

    result = runner.invoke(agents_app, ["rm", "worker-claude", "--force"])
    assert result.exit_code == 0, result.output
    assert received == {"name": "worker-claude", "force": True}


def test_cli_rm_default_force_is_false(monkeypatch, runner: CliRunner) -> None:
    """When --force is omitted, rm_agent receives force=False."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    received: dict = {}

    def fake_rm_agent(name: str, *, force: bool = False):
        received["force"] = force
        return dispatch.RmResult(
            name=name, provider="claude", claude_exit=0,
            force=force, registry_changed=True,
        )

    monkeypatch.setattr(dispatch, "rm_agent", fake_rm_agent)

    result = runner.invoke(agents_app, ["rm", "worker-claude"])
    assert result.exit_code == 0, result.output
    assert received == {"force": False}


def test_cli_rm_help_mentions_force_consequences(runner: CliRunner) -> None:
    """rm --help spells out the orphan-supervisor warning."""
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["rm", "--help"])
    assert result.exit_code == 0
    lowered = result.output.lower()
    assert "orphan" in lowered
    assert "force" in lowered


# ---------------------------------------------------------------------------
# reconcile
# ---------------------------------------------------------------------------


def test_cli_reconcile_json_flag_emits_json(
    monkeypatch, runner: CliRunner
) -> None:
    """--json forces JSON output and the payload round-trips through jq."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    def fake_reconcile():
        return dispatch.ReconcileResult(
            scanned=2,
            orphaned=[{"name": "x", "provider": "claude", "id": "abcd1234"}],
            recovered=[],
            skipped=[
                {"name": "y", "provider": "gemini",
                 "reason": "us4-gemini-not-shipped"},
            ],
            errors=[],
        )

    monkeypatch.setattr(dispatch, "reconcile_agents", fake_reconcile)

    result = runner.invoke(agents_app, ["reconcile", "--json"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["scanned"] == 2
    assert payload["orphaned"][0]["name"] == "x"
    assert payload["skipped"][0]["reason"] == "us4-gemini-not-shipped"


def test_render_reconcile_human_shows_changes_and_rollup() -> None:
    """The human-readable renderer prints one line per change + a roll-up.

    Tested by calling the helper directly because Typer's CliRunner
    cannot expose a TTY-like stdout, so the dispatch's TTY-detection
    branch is unreachable from CliRunner. Calling the helper directly
    side-steps that limitation without changing the production code's
    behavior.
    """
    import io

    from fno.agents import dispatch
    from fno.agents.cli import render_reconcile_human

    result = dispatch.ReconcileResult(
        scanned=3,
        orphaned=[{"name": "a", "provider": "claude", "id": "abcd1234"}],
        recovered=[{"name": "b", "provider": "codex", "id": "uuid-here"}],
        skipped=[
            {"name": "c", "provider": "gemini",
             "reason": "us4-gemini-not-shipped"},
        ],
        errors=[],
    )

    buf = io.StringIO()
    render_reconcile_human(result, out=buf)
    output = buf.getvalue()
    assert "a (claude/abcd1234): live → orphaned" in output
    assert "b (codex/uuid-here): orphaned → live" in output
    assert "c (gemini): skipped (us4-gemini-not-shipped)" in output
    assert "3 entries scanned: 1 orphaned, 1 recovered, 1 skipped" in output


def test_cli_reconcile_dispatches_to_json_when_not_tty(
    monkeypatch, runner: CliRunner
) -> None:
    """Mirroring `fno agents list`: non-TTY stdout auto-emits JSON."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    def fake_reconcile():
        return dispatch.ReconcileResult(
            scanned=1,
            orphaned=[{"name": "a", "provider": "claude", "id": "abcd1234"}],
        )

    monkeypatch.setattr(dispatch, "reconcile_agents", fake_reconcile)
    # CliRunner already runs without a TTY; do not force isatty=True.

    result = runner.invoke(agents_app, ["reconcile"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["scanned"] == 1


def test_cli_reconcile_surfaces_dispatch_error(
    monkeypatch, runner: CliRunner
) -> None:
    """DispatchAskError surfaces via stderr; exit code propagates."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    def fake_reconcile():
        raise dispatch.DispatchAskError(
            "registry read failed: boom", exit_code=12
        )

    monkeypatch.setattr(dispatch, "reconcile_agents", fake_reconcile)

    result = runner.invoke(agents_app, ["reconcile"])
    assert result.exit_code == 12
    assert "registry read failed" in result.output


# ---------------------------------------------------------------------------
# attach
# ---------------------------------------------------------------------------


def test_cli_attach_claude_propagates_exit(
    monkeypatch, runner: CliRunner
) -> None:
    """cmd_attach exits with claude's exit code on detach."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    def fake_attach(name: str):
        return dispatch.AttachResult(
            name=name, provider="claude", exit_code=0
        )

    monkeypatch.setattr(dispatch, "attach_agent", fake_attach)

    result = runner.invoke(agents_app, ["attach", "worker-claude"])
    assert result.exit_code == 0


def test_cli_attach_codex_exits_13(monkeypatch, runner: CliRunner) -> None:
    """codex attach surfaces exit 13 from AttachResult."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    def fake_attach(name: str):
        return dispatch.AttachResult(
            name=name, provider="codex", exit_code=13
        )

    monkeypatch.setattr(dispatch, "attach_agent", fake_attach)

    result = runner.invoke(agents_app, ["attach", "worker-codex"])
    assert result.exit_code == 13


def test_cli_attach_propagates_dispatch_error(
    monkeypatch, runner: CliRunner
) -> None:
    """DispatchAskError-raising attach surfaces exit code 2."""
    from fno.agents import dispatch
    from fno.agents.cli import agents_app

    def fake_attach(name: str):
        raise dispatch.DispatchAskError(
            f"agent {name!r} not found in registry", exit_code=2
        )

    monkeypatch.setattr(dispatch, "attach_agent", fake_attach)

    result = runner.invoke(agents_app, ["attach", "ghost"])
    assert result.exit_code == 2
    assert "not found" in result.output


# ---------------------------------------------------------------------------
# ping (informational stub now; no more _NOT_IMPLEMENTED marker)
# ---------------------------------------------------------------------------


def test_cli_ping_returns_zero(runner: CliRunner) -> None:
    """ping now prints a deferral notice and exits 0."""
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["ping"])
    assert result.exit_code == 0
    assert "future story" in result.output


def test_cli_ping_no_longer_carries_phase1_marker(runner: CliRunner) -> None:
    """The Phase 1 scaffold language is gone (task 2.2 invariant)."""
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["ping"])
    assert "Phase 1 scaffold" not in result.output
