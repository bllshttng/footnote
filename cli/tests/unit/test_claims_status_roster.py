"""The roster-aware `claim status` verdict.

Drives `read_roster` directly so these cases stay about the verdict logic:
a patchy roster that names nobody on this node reads free with
`roster_coverage: degraded`; an unresolved row whose worktree names this
node still fails closed as unknown.
"""
from __future__ import annotations

import json
import time as _time

import pytest
from typer.testing import CliRunner

from fno.claims.cli import RosterReading, cli
from fno.claims import roster as roster_module


runner = CliRunner()

NODE = "node:ac1-node"


def _unresolved(name: str, cwd: str) -> dict:
    return {"name": name, "state": "working", "cwd": cwd, "row_id": name}


def _reading(rows_scanned: int, unresolved: list[dict]) -> RosterReading:
    return RosterReading(True, rows_scanned, {}, "", {}, len(unresolved), tuple(unresolved))


def test_roster_reader_module_is_authority():
    from fno.claims import cli as claims_cli

    assert claims_cli.RosterReading is roster_module.RosterReading
    assert claims_cli.read_roster is roster_module.read_roster
    assert claims_cli._finished_row_states is roster_module._finished_row_states
    assert claims_cli._transcript_activity is roster_module._transcript_activity
    assert claims_cli._worker_reachability is roster_module._worker_reachability


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


# --- x-dead: the crosscheck dates the transcript and hedges its basis ------


def _workers(*workers: dict) -> RosterReading:
    return RosterReading(True, len(workers), {NODE.removeprefix("node:"): list(workers)})


def _install_facts(monkeypatch, epoch):
    from fno.agents.watchdog import TailFacts

    monkeypatch.setattr(
        "fno.agents.watchdog.tail_facts",
        lambda *_a, **_kw: TailFacts(
            records=None, last_event_epoch=epoch, tail_text="", last_role="assistant",
            last_text="working the task", pr_polls=None,
        ),
    )


def test_an_undatable_transcript_never_renders_a_live_worker(cwd_tmp, roster, monkeypatch):
    # Task 1.1, the measured wrong answer: a done row whose transcript could
    # not be dated rendered "UNCLAIMED but a live worker is on this node".
    # UNKNOWN is its own arm: no worked_by, and the line says so.
    _install_facts(monkeypatch, None)
    roster(_workers({"name": "bp-0396", "state": "done", "cwd": "/wt/ac1-node",
                     "row_id": "bp-0396"}))
    r = runner.invoke(cli, ["status", NODE, "--json"])
    assert r.exit_code == 0, r.output
    info = json.loads(r.output)
    assert "worked_by" not in info
    assert "basis" not in info
    r = runner.invoke(cli, ["status", NODE])
    assert "UNCLAIMED but a live worker" not in r.output
    assert "could not be dated" in r.output


def test_a_stale_working_word_yields_to_a_done_tail(cwd_tmp, roster, monkeypatch):
    # Measured live 15:1xZ: t-b7f8-reaper-keep-rules read parked in
    # `fno agents list` while this reader said live from the same row's
    # stale `working` word. The transcript outranks the word.
    _install_facts(monkeypatch, _time.time() - 3 * 3600)
    roster(_workers({"name": "t-b7f8-reaper-keep-rules", "state": "working",
                     "cwd": "/wt/ac1-node", "row_id": "t-b7f8"}))
    r = runner.invoke(cli, ["status", NODE, "--json"])
    assert r.exit_code == 0, r.output
    info = json.loads(r.output)
    assert "worked_by" not in info
    r = runner.invoke(cli, ["status", NODE])
    assert "UNCLAIMED but a live worker" not in r.output


def test_degraded_coverage_hedges_the_basis(cwd_tmp, roster, monkeypatch):
    # Task 2.1, the 31-of-53 specimen: a settled `basis=live-worker` beside
    # `roster_coverage: degraded` hid the hedge. The basis itself now carries
    # it, and the stderr line names the fraction.
    _install_facts(monkeypatch, _time.time())
    unresolved = [_unresolved(f"t-other-{i}", f"/wt/other-{i}") for i in range(31)]
    roster(RosterReading(True, 53, {NODE.removeprefix("node:"): [
        {"name": "t-live", "state": "working", "cwd": "/wt/ac1-node", "row_id": "t-live"},
    ]}, "", {}, len(unresolved), tuple(unresolved)))
    r = runner.invoke(cli, ["status", NODE, "--json"])
    assert r.exit_code == 0, r.output
    info = json.loads(r.output)
    assert info["worked_by"] == ["t-live"]
    assert info["basis"] == "live-worker-degraded-coverage"
    assert info["roster_coverage"] == "degraded"
    r = runner.invoke(cli, ["status", NODE])
    assert "UNCLAIMED but a live worker" in r.output
    assert "coverage degraded: 31 of 53 rows unresolved" in r.output
