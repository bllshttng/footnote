"""`fno help` shorthand legend (Phase 3, ab-a04f3f1a, US3).

The legend is the discoverability half of the short-flag convention: the
global UPPERCASE register and the per-command ``-p`` meanings live behind
``fno help shorthands``, and the bare ``fno help`` output points at it.
"""
from __future__ import annotations

import re

from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()

_ANSI = re.compile(r"\x1b\[[0-9;]*m")


def _clean(text: str) -> str:
    return _ANSI.sub("", text)


def test_help_shorthands_prints_global_register() -> None:
    result = runner.invoke(app, ["help", "shorthands"])
    assert result.exit_code == 0, result.output
    out = _clean(result.output)
    for short, long in [
        ("-J", "--json"),
        ("-A", "--all"),
        ("-F", "--force"),
        ("-N", "--dry-run"),
        ("-R", "--reason"),
        ("-Y", "--yolo"),
    ]:
        assert short in out and long in out, f"register pair {short} {long} missing"


def test_help_shorthands_documents_p_meanings() -> None:
    """-p is the deliberately overloaded letter; the legend must disambiguate."""
    out = _clean(runner.invoke(app, ["help", "shorthands"]).output)
    for meaning in ["provider", "priority", "project", "phase", "pr-number"]:
        assert meaning in out, f"-p meaning '{meaning}' missing from legend"


def test_help_shorthands_documents_canonical_spellings() -> None:
    out = _clean(runner.invoke(app, ["help", "shorthands"]).output)
    assert "--session-id" in out
    assert "--pr-number" in out
    assert "deprecated" in out


def test_bare_help_points_at_shorthands() -> None:
    result = runner.invoke(app, ["help"])
    assert result.exit_code == 0, result.output
    assert "fno help shorthands" in _clean(result.output)
