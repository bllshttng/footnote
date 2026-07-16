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


def test_agents_help_advertises_resume_hides_trace(runner: CliRunner) -> None:
    """x-71b6 tiering: ``resume`` is advertised; ``trace`` is hidden from the
    listing but still invocable (its own --help works, see below)."""
    code, out = _run(runner, "--help")
    assert code == 0
    assert "resume" in out
    # trace is display-hidden now; it must not appear as a listed command.
    assert not re.search(r"^\s*trace\b", out, re.MULTILINE), (
        "hidden verb 'trace' leaked into the advertised agents --help listing"
    )


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


# x-71b6: the advertised `fno agents` menu (the eight In-N-Out verbs).
_ADVERTISED_AGENTS_VERBS = {
    "spawn", "list", "logs", "watch", "attach", "stop", "resume", "status",
}


def test_agents_help_advertises_only_the_eight_menu_verbs(runner: CliRunner) -> None:
    """AC1-HP: `fno agents --help` lists at most 12 verbs, and exactly the
    advertised set (spawn/list/logs/watch/attach/stop/resume/status)."""
    import click
    import typer.main

    group = typer.main.get_command(agents_app)
    ctx = click.Context(group)
    listed = [
        name
        for name in group.list_commands(ctx)
        if not (cmd := group.get_command(ctx, name)) or not cmd.hidden
    ]
    assert set(listed) == _ADVERTISED_AGENTS_VERBS, (
        f"advertised agents verbs drifted: {sorted(listed)}"
    )
    assert len(listed) <= 12


@pytest.mark.parametrize("verb", ["ask", "whoami", "top", "ping", "rm", "reconcile", "trace"])
def test_hidden_python_agents_verbs_stay_invocable(runner: CliRunner, verb: str) -> None:
    """AC2-ERR: a display-hidden Python agents verb still runs its own --help."""
    code, out = _run(runner, verb, "--help")
    assert code == 0, f"hidden verb {verb!r} --help failed: {out}"
