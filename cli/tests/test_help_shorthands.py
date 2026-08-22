"""`fno help` shorthand legend (Phase 3, US3).

The legend is the discoverability half of the short-flag convention: the
global UPPERCASE register and the per-command ``-p`` meanings live behind
``fno help shorthands``, and the bare ``fno help`` output points at it.

The substring assertions below check only that the legend SAYS something, and
that let three dead rows sit here: ``fno agents ask -p provider`` (``-p`` is a
loud tombstone off ``spawn``), ``fno gate verify`` (no such command anywhere),
and ``fno backlog done -p pr-number (-l link, -m note)`` (that flag surface
belongs to the deprecated root ``fno done``). A substring of prose passes
whether or not the flag is real, so the row-level checks at the bottom read
the live command tree instead.
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
    for meaning in ["priority", "project", "pr-number", "kind", "type"]:
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


# Two renderers are in play: plain click prints `-F, --force`, rich-formatted
# typer prints `--pr-number  -p`. Match a short only where a long sits beside
# it, in either order, so prose hyphens in the docstring are never mistaken for
# a flag.
_SHORT_BESIDE_LONG = re.compile(
    r"(?:(-[a-zA-Z])[,\s]+--[a-z][\w-]*)|(?:--[a-z][\w-]*\s+(-[a-zA-Z])(?![\w-]))"
)


def _shorts_on(path: str) -> set[str]:
    """Short options click reports for a command path, read off its own --help."""
    result = runner.invoke(app, [*path.split(), "--help"])
    assert result.exit_code == 0, f"`fno {path} --help` exited {result.exit_code}"
    return {
        short
        for pair in _SHORT_BESIDE_LONG.findall(_clean(result.output))
        for short in pair
        if short
    }


def test_backlog_done_row_matches_the_command() -> None:
    """The legend must not hand `backlog done` the deprecated root's flags.

    `fno backlog done` closes a node: --skip-stamp / -F --force / -R --reason,
    and nothing else. The PR-metadata surface (-p pr-number, -l link, -m note,
    --backfill) belongs to the deprecated root `fno done`, which is a separate
    command with a separate body. The legend claimed the pair were one row.
    """
    from fno.cli import SHORTHAND_LEGEND

    assert _shorts_on("backlog done") == {"-F", "-R"}
    assert _shorts_on("done") >= {"-p", "-l", "-m"}
    assert "fno backlog done  " not in SHORTHAND_LEGEND


# `fno agents ask` execs the fno-agents Rust front for --help, replacing this
# process. Probing it in-process does not fail the test, it terminates the
# pytest worker mid-run, so the row is checked by hand instead: the Python
# command carries -H/-Y/-c/-t and no -p, and client.rs refuses `-p` off `spawn`
# with a tombstone message.
_EXEC_SHIM_ROWS = {"agents ask"}


def test_legend_names_no_dead_command() -> None:
    """Every `fno <path>` a legend row names still resolves."""
    from fno.cli import SHORTHAND_LEGEND

    rows = [
        line.strip()[len("fno ") :].split("  ")[0].strip()
        for line in SHORTHAND_LEGEND.splitlines()
        if line.startswith("  fno ")
    ]
    assert len(rows) >= 5, "legend rows stopped parsing; fix the parse, not the legend"
    for cell in rows:
        cell = re.sub(r"\s*\(.*\)\s*$", "", cell).strip()
        parts = cell.split()
        # A row may name a family: `backlog add/idea/update/intake`.
        leaves = parts[-1].split("/") if "/" in parts[-1] else [parts[-1]]
        for leaf in leaves:
            path = " ".join(parts[:-1] + [leaf])
            if path in _EXEC_SHIM_ROWS:
                continue
            result = runner.invoke(app, [*path.split(), "--help"])
            assert result.exit_code == 0, f"legend names `fno {path}`, which does not resolve"
