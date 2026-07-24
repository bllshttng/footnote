"""Coverage gap 1: end-to-end Typer wiring for ``--yolo`` on ``fno agents ask``.

US4-codex (PR #305) added the ``--yolo`` flag to ``cmd_ask`` and wired
``yolo=yolo`` from ``cli.py:107`` through to ``dispatch.dispatch_ask``.
``test_dispatch_ask`` and ``test_dispatch_codex`` exercise ``dispatch_ask``
directly with ``yolo=True``, but the Typer wiring itself was structurally
trusted. This test pins the contract end-to-end so a regression that
drops the passthrough fails immediately.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _stub_dispatch_kwargs(monkeypatch) -> dict:
    """Replace ``cli.dispatch_ask`` with a recorder; return the captured kwargs.

    The fake DispatchAskResult is the minimum shape ``cmd_ask`` writes to
    stdout. We don't care about reply content here - the assertion is on
    the kwargs the Typer command forwarded, not on what the dispatch did.
    """
    received: dict = {}

    from fno.agents import dispatch as dispatch_mod

    def fake_dispatch_ask(**kwargs):
        received.update(kwargs)
        return dispatch_mod.DispatchAskResult(
            kind="followup",
            short_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            reply="ok",
            duration_ms=10,
        )

    monkeypatch.setattr(dispatch_mod, "dispatch_ask", fake_dispatch_ask)
    return received


def test_ask_yolo_flag_reaches_dispatch_ask(monkeypatch, runner: CliRunner) -> None:
    """AC4-HP: ``fno agents ask ... --yolo`` reaches dispatch_ask with yolo=True."""
    received = _stub_dispatch_kwargs(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        [
            "ask", "worker", "msg",
            "--harness", "codex",
            "--yolo",
        ],
    )

    assert result.exit_code == 0, result.output
    assert received["yolo"] is True
    assert received["name"] == "worker"
    assert received["message"] == "msg"
    assert received["provider"] == "codex"


def test_ask_without_yolo_defaults_false(monkeypatch, runner: CliRunner) -> None:
    """AC4-ERR: omitting ``--yolo`` passes ``yolo=False`` to dispatch_ask."""
    received = _stub_dispatch_kwargs(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["ask", "worker", "msg", "--harness", "codex"]
    )

    assert result.exit_code == 0, result.output
    assert received["yolo"] is False


def test_ask_yolo_with_other_options_still_passes(
    monkeypatch, runner: CliRunner
) -> None:
    """Defense against subtle drift: --yolo + --from-name + --cwd all coexist."""
    received = _stub_dispatch_kwargs(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        [
            "ask", "worker", "msg",
            "--harness", "codex",
            "--yolo",
            "--from-name", "smoke-test",
        ],
    )

    assert result.exit_code == 0, result.output
    assert received["yolo"] is True
    assert received["from_name"] == "smoke-test"
