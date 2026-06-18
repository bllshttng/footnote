"""Tests that `fno agents --help` lists the new observability verbs.

Task 3.5 from 2026-05-22-fno-agents-observability.md — closes the
GAP-1 finding from the sigma-review integration-test analyzer: the
plan named ``test_agents_cli_help.py`` but it never landed in the
original Wave 3 commits.

Verifies via Typer's CliRunner that:
- `fno agents --help` lists ``trace`` and ``resume`` in its subcommand
  table.
- `fno agents trace --help` shows the locked option set.
- `fno agents resume --help` shows ``--print-command``.

Typer renders help via Rich, which wraps long lines and intersperses
ANSI escapes. We strip ANSI before substring-checking so the test is
robust to terminal-width-dependent rendering.
"""
from __future__ import annotations

import re

import pytest
from typer.testing import CliRunner

from fno.agents.cli import agents_app


_ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def _strip_ansi(s: str) -> str:
    """Remove ANSI escape sequences and collapse whitespace for substring checks."""
    return _ANSI_RE.sub("", s)


@pytest.fixture
def runner() -> CliRunner:
    # Set terminal_width high so Rich doesn't wrap option names mid-flag.
    return CliRunner()


def _run(runner: CliRunner, *args: str) -> tuple[int, str]:
    result = runner.invoke(agents_app, list(args), terminal_width=200)
    return result.exit_code, _strip_ansi(result.stdout)


def test_agents_help_lists_trace_and_resume(runner: CliRunner) -> None:
    """Both new verbs must appear under ``Commands:`` in agents --help."""
    code, out = _run(runner, "--help")
    assert code == 0
    assert "trace" in out
    assert "resume" in out


def test_agents_trace_help_shows_locked_options(runner: CliRunner) -> None:
    """Spec-locked option set for trace must be documented in --help."""
    code, out = _run(runner, "trace", "--help")
    assert code == 0
    for option in ("--json", "--since", "--request-id", "--all", "--limit"):
        assert option in out, f"missing option {option} in trace --help"


def test_agents_resume_help_shows_print_command(runner: CliRunner) -> None:
    """`--print-command` must be documented in resume --help."""
    code, out = _run(runner, "resume", "--help")
    assert code == 0
    assert "--print-command" in out
