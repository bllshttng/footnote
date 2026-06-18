"""Test for the megawalk stub callback (task 2.4).

After walker deletion, `fno megawalk` (bare, no subcommand) must:
- Exit with code 12 (preserves prior exit-12 contract, pinned by smoke test)
- Mention the new front door: `fno-agents loop run --driver megawalk`
- Mention the /megawalk skill

The `watch` subcommand must still be registered and callable.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest
from typer.testing import CliRunner


def _get_app():
    from fno.megawalk import app
    return app


def test_AC1_HP_bare_callback_exits_12_with_new_front_door_message():
    """AC1-HP: bare `fno megawalk` exits 12 and mentions the Rust loop front door."""
    runner = CliRunner()
    app = _get_app()
    result = runner.invoke(app, [], catch_exceptions=False)
    assert result.exit_code == 12, (
        f"expected exit 12, got {result.exit_code}; output: {result.output}"
    )
    combined = (result.output or "") + (result.stderr if hasattr(result, "stderr") and result.stderr else "")
    assert "fno-agents loop run --driver megawalk" in combined, (
        f"new front door not mentioned in output: {combined!r}"
    )


def test_AC2_HP_watch_subcommand_still_registered():
    """AC2-HP: `fno megawalk watch --help` exits 0 - watch command is still present."""
    runner = CliRunner()
    app = _get_app()
    result = runner.invoke(app, ["watch", "--help"], catch_exceptions=False)
    assert result.exit_code == 0, (
        f"watch --help failed with {result.exit_code}; output: {result.output}"
    )
    assert "watch" in result.output.lower()


def test_AC3_ERR_unknown_subcommand_exits_nonzero():
    """AC3-ERR: unknown subcommand exits non-zero (Typer default error handling)."""
    runner = CliRunner()
    app = _get_app()
    result = runner.invoke(app, ["nonexistent-subcommand"])
    assert result.exit_code != 0, (
        f"unknown subcommand should exit non-zero, got {result.exit_code}"
    )
