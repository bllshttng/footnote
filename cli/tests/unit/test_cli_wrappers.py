"""Cross-wrapper smoke tests: all 8 new fno subcommands respond to --help.

Task 02.2 of plan 2026-05-11-abi-cli-promotion-wrappers.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()
_ENV = {"COLUMNS": "240", "NO_COLOR": "1", "TERM": "dumb"}


@pytest.mark.parametrize(
    "argv",
    [
        # gate-set and phase-verify removed by the control-plane collapse
        # wedge (ab-d0337fbc): the `fno gate` sub-app is gone and `fno phase`
        # keeps only kill-check.
        ["pr", "verify", "--help"],
        ["pr", "rebase", "--help"],
        ["event", "verify-evidence", "--help"],
        ["phase", "kill-check", "--help"],
        ["executor", "resolve", "--help"],
        ["notify", "--help"],
    ],
    ids=[
        "pr-verify",
        "pr-rebase",
        "event-verify-evidence",
        "phase-kill-check",
        "executor-resolve",
        "notify",
    ],
)
def test_new_subcommand_help_renders(argv):
    """AC1-HP: every new subcommand responds to --help with exit 0."""
    result = runner.invoke(app, argv, env=_ENV)
    assert result.exit_code == 0, (
        f"argv={argv!r} exited {result.exit_code}; output:\n{result.output}"
    )
    assert len(result.output) > 0, f"argv={argv!r} produced empty output"


def test_top_level_help_lists_new_subapps():
    """AC4-UI: phase/executor/notify are registered and reachable.

    Under x-71b6 In-N-Out tiering they are hidden from the curated `--help`
    menu but still listed by the full-surface door `fno help --all`.
    """
    result = runner.invoke(app, ["help", "--all"], env=_ENV)
    assert result.exit_code == 0
    for noun in ("phase", "executor", "notify"):
        assert noun in result.output, f"missing {noun!r} in `fno help --all` output"
