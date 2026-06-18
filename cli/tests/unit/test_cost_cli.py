"""Tests for the `fno cost` verb (ab-18563bcc, resolves cv-cea90c09).

`fno cost` is a thin passthrough to the in-package
``fno.cost._session_cost.main`` argparse CLI (the former
scripts/metrics/session-cost.py). These pin that the verb is registered, runs
from in-package Python, and propagates the underlying exit code - without a
repo-root script.
"""
from __future__ import annotations

from typer.testing import CliRunner


def test_cost_verb_registered_and_help_exits_zero() -> None:
    from fno.cli import app

    result = CliRunner().invoke(app, ["cost", "--help"])
    assert result.exit_code == 0, result.output
    assert "cost" in result.output.lower()


def test_cost_forwards_args_to_session_cost_main(monkeypatch) -> None:
    # The verb must delegate to the in-package main with the args verbatim,
    # not shell out to a script.
    import sys

    import fno.cost._session_cost as sc
    from fno.cli import app

    seen: dict[str, list[str]] = {}

    def fake_main() -> None:
        seen["argv"] = list(sys.argv)

    monkeypatch.setattr(sc, "main", fake_main)
    result = CliRunner().invoke(app, ["cost", "--by-provider", "--json"])
    assert result.exit_code == 0, result.output
    # sys.argv[0] is the synthetic "fno cost"; the rest are forwarded verbatim.
    assert seen["argv"][1:] == ["--by-provider", "--json"]


def test_cost_propagates_nonzero_exit(monkeypatch) -> None:
    import fno.cost._session_cost as sc
    from fno.cli import app

    def boom() -> None:
        raise SystemExit(3)

    monkeypatch.setattr(sc, "main", boom)
    result = CliRunner().invoke(app, ["cost", "bogus-session"])
    assert result.exit_code == 3
