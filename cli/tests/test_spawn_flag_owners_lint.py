"""The spawn-flag-owners lint bites on demand, in the tree it gates.

A gate that degrades to a pass was never a gate. ``menu-caps`` has this
shape of test; the flag-ownership ratchet gets the same: run the real check
against the real parser, then remove a row and add a bogus row, and require
a named failure each way.

Run: cd cli && uv run pytest tests/test_spawn_flag_owners_lint.py -v
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.agents import spawn_flag_owners


def _invoke_lint() -> object:
    from fno.cli import app

    return CliRunner().invoke(app, ["doctor", "lint", "spawn-flag-owners"])


def test_passes_on_the_live_tree() -> None:
    result = _invoke_lint()
    assert result.exit_code == 0, result.output
    assert "42/42" in result.output


def test_a_row_removal_fails_naming_the_flag(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delitem(spawn_flag_owners.FLAG_OWNERS, "--account")
    result = _invoke_lint()
    assert result.exit_code != 0
    assert "--account" in result.output


def test_a_bogus_row_fails_as_stale(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setitem(
        spawn_flag_owners.FLAG_OWNERS,
        "--zzz-stale",
        spawn_flag_owners.FlagOwner(spawn_flag_owners.FNO, "bogus"),
    )
    result = _invoke_lint()
    assert result.exit_code != 0
    assert "--zzz-stale" in result.output
