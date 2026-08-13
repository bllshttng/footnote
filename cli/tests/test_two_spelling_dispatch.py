"""Runtime dispatch proofs for the two-spelling canonicalization.

``test_short_flag_convention.py`` proves the declaration shape (canonical
long visible, legacy long hidden) via static AST scan, and
``tests/unit/test_flag_aliases.py`` proves the merge helper's semantics in
isolation. These tests drive the REAL root app, so each touched sub-app
imports, registers, and parses - the one thing neither of the other two can
see.

The `reality-check gh` surface these tests once used as the representative
command is gone (the whole sub-app was removed as dead). The surfaces below
are not: they still declare a hidden deprecated alias, so their registration
and help visibility still need a runtime proof.
"""
from __future__ import annotations

import re

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()

_ANSI = re.compile(r"\x1b\[[0-9;]*m")


def _clean(text: str) -> str:
    """Strip ANSI styling so substring asserts see contiguous flag names."""
    return _ANSI.sub("", text)


HELP_SURFACES: dict[str, list[str]] = {
    "review": ["review", "--help"],
    "backlog-cost": ["backlog", "cost", "--help"],
    "retro-run": ["retro", "run", "--help"],
    "worker-review": ["worker", "review", "--help"],
    "worker-external": ["worker", "external", "--help"],
    "done": ["done", "--help"],
}


@pytest.mark.parametrize(
    "argv",
    list(HELP_SURFACES.values()),
    ids=list(HELP_SURFACES.keys()),
)
def test_surface_registers(argv: list[str]) -> None:
    """Each touched sub-app imports and Click accepts its flag decls."""
    result = runner.invoke(app, argv)
    assert result.exit_code == 0, result.output


@pytest.mark.parametrize(
    "argv,canonical,legacy",
    [
        (["backlog", "cost", "--help"], "--session-id", "--session"),
        (["worker", "external", "--help"], "--pr-number", "--pr"),
    ],
    ids=["backlog-cost", "worker-external"],
)
def test_legacy_spelling_hidden_from_help(
    argv: list[str], canonical: str, legacy: str
) -> None:
    """Help shows the canonical spelling and hides the deprecated alias.

    The legacy spelling is a prefix of the canonical one, so a plain
    substring check would match the canonical form. Instead: every
    occurrence of the legacy spelling must be part of a canonical
    occurrence (equal counts means no standalone legacy flag in help).
    """
    result = runner.invoke(app, argv)
    assert result.exit_code == 0, result.output
    out = _clean(result.output)
    assert canonical in out, f"{canonical} missing from {argv}"
    assert out.count(legacy) == out.count(canonical), (
        f"deprecated {legacy} visible in {argv}"
    )
