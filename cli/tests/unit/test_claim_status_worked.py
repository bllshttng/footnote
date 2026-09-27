"""Machine-readable live-worker evidence for claim status."""
from __future__ import annotations

import json
import time
from datetime import datetime, timezone
from types import SimpleNamespace

from typer.testing import CliRunner

from fno.agents.reachability import REACHABLE, UNREACHABLE
from fno.claims.cli import RosterReading, cli
from fno.claims.roster import _worker_reachability
from fno.graph.statuses import live_worked_node_ids as _real_live_worked_node_ids


runner = CliRunner()


def _stop_row(stopped_at=None, state="working") -> dict:
    return {
        "name": "stop-worker",
        "state": state,
        "cwd": "/worktrees/ac1-node",
        "row_id": "session-stop",
        "stopped_at": stopped_at,
    }


def _fresh_tail(monkeypatch, last_event_epoch: float) -> None:
    from fno.agents.watchdog import TailFacts

    monkeypatch.setattr(
        "fno.agents.watchdog.tail_facts",
        lambda *_a, **_kw: TailFacts(
            records=None,
            last_event_epoch=last_event_epoch,
            tail_text="progress update",
            last_role="assistant",
            last_text="progress update",
        ),
    )
    monkeypatch.setattr(
        "fno.agents.watchdog.harness_for_session", lambda *_a, **_kw: "claude"
    )


def test_a_stop_newer_than_the_tail_falsifies_liveness(monkeypatch):
    """A claude row fno just stopped keeps a seconds-old tail; the stop stamp
    must outrank the fresh-tail cancel so the node reads free at once."""
    now = time.time()
    _fresh_tail(monkeypatch, now - 30)
    stamp = datetime.fromtimestamp(now - 10, timezone.utc).isoformat()

    verdict = _worker_reachability(_stop_row(stopped_at=stamp))

    assert verdict.verdict == UNREACHABLE
    assert verdict.basis.startswith("stopped:")


def test_a_tail_event_after_the_stop_still_reads_reachable(monkeypatch):
    """A tail event strictly after the stop is a resumed session; the stop
    stamp does not condemn it."""
    now = time.time()
    _fresh_tail(monkeypatch, now - 5)
    stamp = datetime.fromtimestamp(now - 10, timezone.utc).isoformat()

    verdict = _worker_reachability(_stop_row(stopped_at=stamp))

    assert verdict.verdict == REACHABLE


def test_a_missing_or_unparseable_stop_stamp_changes_nothing(monkeypatch):
    """No stop record (or a torn one) leaves origin/main behavior intact."""
    now = time.time()
    _fresh_tail(monkeypatch, now - 5)
    for stopped_at in (None, "", "not-a-date"):
        verdict = _worker_reachability(_stop_row(stopped_at=stopped_at))
        assert verdict.verdict == REACHABLE, stopped_at


def _graph_entry(monkeypatch, node_id: str = "ac1-node", session_id: str = "session-1") -> None:
    """Pin the graph read to one node with one open-phase session row, and
    put the real overlay back over the conftest hermetic stub (these tests
    ARE about the join, so the stub's empty answer would be the zero under
    test)."""
    entry = {
        "id": node_id,
        "status": "ready",
        "sessions": [
            {
                "phase": "do",
                "harness": "claude",
                "session_id": session_id,
                "started_at": "2026-09-11T00:00:00Z",
            }
        ],
    }
    monkeypatch.setattr(
        "fno.graph.store.read_nodes_by_ids",
        lambda *_a, **_kw: {"entries": [entry]},
    )
    monkeypatch.setattr(
        "fno.graph.statuses.live_worked_node_ids", _real_live_worked_node_ids
    )


def test_ac6_edge_claim_status_names_live_worker(monkeypatch):
    row = {
        "name": "bp-worker",
        "state": "working",
        "cwd": "/worktrees/ac1-node",
        "row_id": "session-1",
    }
    reading = RosterReading(True, 1, {"ac1-node": [row]}, "", {"session-1": row}, 0, ())
    monkeypatch.setattr("fno.claims.cli.read_roster", lambda **_kw: reading)
    _graph_entry(monkeypatch)
    monkeypatch.setattr(
        "fno.claims.cli._claims_core.claim_status",
        lambda **_kw: {"key": "node:ac1-node", "state": "free"},
    )

    result = runner.invoke(cli, ["status", "node:ac1-node", "--json"])

    assert result.exit_code == 0, result.output
    info = json.loads(result.stdout)
    # A positively-live worker on an unheld node is never `free`: the
    # composite verdict reads unknown so dispatch consumers refuse it.
    assert info["state"] == "unknown"
    assert info["worked_by"] == ["bp-worker"]
    assert info["basis"] == "live-worker"


def test_ac1_hp_join_resolves_an_unresolved_row_through_the_graph(monkeypatch):
    """The reproduced case as a fixture: a row with no node field whose
    row_id matches an open-phase session row on this node. 68 unresolved of
    133 scanned is the measured wedge shape, and the verdict still turns on
    the one row that names THIS node, never on the ratio."""
    s1 = {
        "name": "king-a792-control",
        "state": "working",
        "cwd": "/Users/bb16/code/footnote/footnote",
        "row_id": "s-1",
    }
    unresolved = tuple(
        {**s1, "row_id": f"s-other-{i}", "cwd": f"/wt/other-{i}"}
        for i in range(1, 68)
    )
    reading = RosterReading(True, 133, {}, "", {"s-1": s1}, 68, (s1,) + unresolved)
    monkeypatch.setattr("fno.claims.cli.read_roster", lambda **_kw: reading)
    _graph_entry(monkeypatch, session_id="s-1")
    monkeypatch.setattr(
        "fno.claims.cli._claims_core.claim_status",
        lambda **_kw: {"key": "node:ac1-node", "state": "free"},
    )

    result = runner.invoke(cli, ["status", "node:ac1-node", "--json"])

    assert result.exit_code == 0, result.output
    info = json.loads(result.stdout)
    assert info["state"] == "unknown"
    assert info["worked_by"] == ["king-a792-control"]
    assert info["basis"] == "live-worker-degraded-coverage"


def test_ac5_err_registry_only_probe_still_answers_the_join(monkeypatch):
    """A probe degraded to the registry-only view still carries attribution:
    the join runs on it and the payload names the degraded probe instead of
    reporting the roster unconsulted."""
    fallback = (
        "claude agents --json: claude binary not found on PATH; "
        "live_status unavailable, falling back to registry-only view"
    )
    row = SimpleNamespace(
        name="reg-worker", state="working", cwd="/worktrees/ac1-node",
        row_id="s-1", pid=None, pid_start_time=None, mux=None, node=None,
    )
    monkeypatch.setattr(
        "fno.agents.watchdog.fleet_rows", lambda **_kw: ([row], [fallback])
    )
    _graph_entry(monkeypatch, session_id="s-1")
    monkeypatch.setattr(
        "fno.claims.cli._claims_core.claim_status",
        lambda **_kw: {"key": "node:ac1-node", "state": "free"},
    )

    result = runner.invoke(cli, ["status", "node:ac1-node", "--json"])

    assert result.exit_code == 0, result.output
    info = json.loads(result.stdout)
    assert info["roster_consulted"] is True
    assert "registry-only" in info["roster_probe"]
    assert info["state"] == "unknown"
    assert info["worked_by"] == ["reg-worker"]


def test_a_closed_phase_row_never_renders_as_unmeasured_noise(monkeypatch):
    """The display field drops what the overlay's worked_by drops: a worker
    whose own phase row closed on this node is not an occupancy candidate
    and must not render as 'unmeasured, never live' in the verdict line
    either."""
    row = {
        "name": "bp-closed",
        "state": "working",
        "cwd": "/worktrees/ac1-node",
        "row_id": "session-1",
    }
    reading = RosterReading(True, 1, {"ac1-node": [row]}, "", {"session-1": row}, 0, ())
    monkeypatch.setattr("fno.claims.cli.read_roster", lambda **_kw: reading)
    entry = {
        "id": "ac1-node",
        "status": "ready",
        "sessions": [
            {
                "phase": "do",
                "harness": "claude",
                "session_id": "session-1",
                "started_at": "2026-09-11T00:00:00Z",
                "ended_at": "2026-09-11T01:00:00Z",
            }
        ],
    }
    monkeypatch.setattr(
        "fno.graph.store.read_nodes_by_ids",
        lambda *_a, **_kw: {"entries": [entry]},
    )
    monkeypatch.setattr(
        "fno.graph.statuses.live_worked_node_ids", _real_live_worked_node_ids
    )
    monkeypatch.setattr(
        "fno.claims.cli._claims_core.claim_status",
        lambda **_kw: {"key": "node:ac1-node", "state": "free"},
    )

    result = runner.invoke(cli, ["status", "node:ac1-node", "--json"])

    assert result.exit_code == 0, result.output
    info = json.loads(result.stdout)
    # The crosscheck provably ran; the absence below is the filter, not a
    # dead instrument.
    assert info["roster_consulted"] is True
    assert info["roster_workers"] == []
    assert "worked_by" not in info
    assert "unmeasured, never live" not in result.output


def test_ac7_edge_unrelated_unresolved_rows_still_answer_free(monkeypatch):
    unresolved = {
        "name": "other-worker",
        "state": "working",
        "cwd": "/worktrees/other-node",
        "row_id": "other-session",
    }
    reading = RosterReading(True, 2, {}, "", {}, 1, (unresolved,))
    monkeypatch.setattr("fno.claims.cli.read_roster", lambda **_kw: reading)
    monkeypatch.setattr(
        "fno.claims.cli._claims_core.claim_status",
        lambda **_kw: {"key": "node:ac1-node", "state": "free"},
    )

    result = runner.invoke(cli, ["status", "node:ac1-node", "--json"])

    assert result.exit_code == 0, result.output
    info = json.loads(result.stdout)
    assert info["state"] == "free"
    assert info["roster_coverage"] == "degraded"
