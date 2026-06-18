"""Tests for `fno agents` Typer subapp scaffold — TDD Red phase.

AC5/AC6 from Task 1.2:
- `fno agents` Typer subapp wired into main CLI; `fno agents --help` prints usage
- Empty `ask` / `list` / `ping` stubs exit 0 and print "not implemented yet"
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def test_agents_app_exists() -> None:
    """fno.agents.cli exports a Typer app named agents_app."""
    from fno.agents.cli import agents_app
    import typer

    assert isinstance(agents_app, typer.Typer)


def test_agents_help_prints_usage(runner: CliRunner) -> None:
    """`fno agents --help` exits 0 and prints usage with all wired commands."""
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["--help"])
    assert result.exit_code == 0
    out = result.output.lower()
    assert "ask" in out
    assert "list" in out
    assert "logs" in out  # US3
    assert "ping" in out


def test_agents_ask_help_is_documented(runner: CliRunner, monkeypatch) -> None:
    """`fno agents ask --help` documents the US1 surface (replaces Phase 1 stub).

    The full US1 ask contract is exercised in test_cmd_ask.py and
    test_dispatch_ask.py; this assertion only guards the help surface.

    CI vs local: Rich (via Typer) renders --help with ANSI escape codes
    and may wrap long option lines into unicode box-drawing characters
    based on detected terminal width. Locally we read a wide terminal;
    CI's runner reports a narrow width and the substring match fails.
    Disable Rich's coloring + force a wide column to keep the assertion
    portable.
    """
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("TERM", "dumb")
    monkeypatch.setenv("COLUMNS", "300")
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["ask", "--help"])
    assert result.exit_code == 0
    # Strip ANSI escape sequences (Rich may still emit them under some
    # Typer versions) before substring matching.
    import re

    stripped = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", result.output).lower()
    assert "--provider" in stripped, stripped[:500]
    assert "--cwd" in stripped, stripped[:500]


def test_agents_list_shipped_in_us3(tmp_path: Path, monkeypatch, runner: CliRunner) -> None:
    """`fno agents list` is no longer a stub — US3 shipped real impl.

    Detailed CLI behavior (filters, JSON shape, exit codes) lives in
    test_cli_list_logs.py; this test only guards against the stub
    accidentally being restored.
    """
    use_tmpdir(monkeypatch, tmp_path)
    # Silence the live-status shellout so the assertion focuses on the
    # CLI itself, not the provider plumbing.
    from fno.agents.providers import claude as claude_mod
    monkeypatch.setattr(
        claude_mod, "claude_agents_json", lambda timeout=3.0: ({}, []),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["list"])
    assert result.exit_code == 0
    assert "not implemented yet" not in result.output.lower()


def test_agents_ping_stub_exits_zero(tmp_path: Path, monkeypatch, runner: CliRunner) -> None:
    """`fno agents ping` exits 0 with the US4-lifecycle deferral notice.

    Task 2.2 converted the Phase-1 ``_NOT_IMPLEMENTED`` marker into a
    deferral message ("(not yet implemented; planned for a future story)").
    The exit code stays 0 so operator workflows that probe the verb
    without expecting real work to happen do not break.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["ping"])
    assert result.exit_code == 0
    assert "not yet implemented" in result.output.lower()
    assert "future story" in result.output.lower()


def test_agents_registered_in_main_cli() -> None:
    """`fno agents` is registered in the top-level CLI's LAZY_SUBCOMMANDS map."""
    from fno.cli import LAZY_SUBCOMMANDS

    assert "agents" in LAZY_SUBCOMMANDS
    entry = LAZY_SUBCOMMANDS["agents"]
    # tuple shape: (import_path, short_help[, options])
    import_path = entry[0]
    assert import_path == "fno.agents.cli:agents_app"
