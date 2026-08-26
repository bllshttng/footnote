"""Cross-wrapper smoke tests: all 8 new fno subcommands respond to --help.

Task 02.2 of plan 2026-05-11-fno-cli-promotion-wrappers.
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
        # wedge (ab-d0337fbc): the `fno gate` sub-app is gone and `fno do phase`
        # keeps only kill-check.
        ["pr", "verify", "--help"],
        ["pr", "rebase", "--help"],
        ["phase", "kill-check", "--help"],
        ["notify", "--help"],
    ],
    ids=[
        "pr-verify",
        "pr-rebase",
        "phase-kill-check",
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
    """AC4-UI: phase/notify are registered and reachable.

    Under x-71b6 In-N-Out tiering they are hidden from the curated `--help`
    menu. They used to be listed by the full-surface door `fno help --all`,
    but the moved-spellings block is gone (d-26002be8: aliases are discovered
    in their own subcommands), so reachability is now proven by invoking each
    deprecated spelling and checking it forwards to its new home.
    """
    for noun in ("phase", "notify"):
        result = runner.invoke(app, [noun, "--help"], env=_ENV)
        assert result.exit_code == 0, (
            f"`fno {noun} --help` exited {result.exit_code}; output:\n{result.output}"
        )
        assert result.output, f"`fno {noun} --help` produced empty output"
