"""The roster-aware `claim status` verdict.

Drives `read_roster` directly so these cases stay about the verdict logic:
a patchy roster that names nobody on this node reads free with
`roster_coverage: degraded`; an unresolved row whose worktree names this
node still fails closed as unknown.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.claims.cli import RosterReading, cli


runner = CliRunner()

NODE = "node:ac1-node"


def _unresolved(name: str, cwd: str) -> dict:
    return {"name": name, "state": "working", "cwd": cwd, "row_id": name}


def _reading(rows_scanned: int, unresolved: list[dict]) -> RosterReading:
    return RosterReading(True, rows_scanned, {}, "", {}, len(unresolved), tuple(unresolved))


@pytest.fixture
def roster(monkeypatch):
    def _install(reading: RosterReading) -> None:
        monkeypatch.setattr("fno.claims.cli.read_roster", lambda *a, **kw: reading)

    return _install


def test_a_patchy_roster_that_names_nobody_reads_free_but_degraded(cwd_tmp, roster):
    unresolved = [_unresolved(f"t-other-{i}", f"/wt/other-{i}") for i in range(64)]
    roster(_reading(129, unresolved))
    r = runner.invoke(cli, ["status", NODE, "--json"])
    assert r.exit_code == 0, r.output
    info = json.loads(r.output)
    assert info["state"] == "free"
    assert info["roster_coverage"] == "degraded"
    assert info["roster_rows_scanned"] == 129
    assert info["roster_rows_unresolved"] == 64
    assert "basis" not in info


def test_the_degraded_human_line_carries_both_numbers(cwd_tmp, roster):
    unresolved = [_unresolved(f"t-other-{i}", f"/wt/other-{i}") for i in range(64)]
    roster(_reading(129, unresolved))
    r = runner.invoke(cli, ["status", NODE])
    assert r.exit_code == 0, r.output
    assert "(129 scanned, 64 unresolved)" in r.output
    assert "roster coverage degraded" in r.output


def test_an_unresolved_row_naming_this_node_still_reads_unknown(cwd_tmp, roster):
    roster(_reading(129, [_unresolved("t-here", "/wt/ac1-node")]))
    r = runner.invoke(cli, ["status", NODE, "--json"])
    assert r.exit_code == 0, r.output
    info = json.loads(r.output)
    assert info["state"] == "unknown"
    assert info["basis"] == "unresolved-roster-row"
    assert "roster_coverage" not in info
    r = runner.invoke(cli, ["status", NODE])
    assert "t-here" in r.output
    assert "Confirm with: fno agents peek t-here" in r.output
