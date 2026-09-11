"""Positive live-worker overlay tests for graph status consumers."""
from __future__ import annotations

import pytest

from fno.claims.roster import RosterReading
from fno.graph.statuses import live_worked_node_ids


def _entry(node_id: str, *, status: str = "ready") -> dict:
    return {
        "id": node_id,
        "status": status,
        "sessions": [
            {
                "phase": "blueprint",
                "harness": "claude",
                "session_id": "session-1",
                "started_at": "2026-09-09T00:00:00Z",
            }
        ],
    }


def _reading(state: str) -> RosterReading:
    row = {
        "name": "bp-worker",
        "state": state,
        "cwd": "/worktrees/ac1-node",
        "row_id": "session-1",
    }
    return RosterReading(True, 1, {}, "", {"session-1": row}, 0, ())


def test_ac1_hp_names_a_live_worker(monkeypatch):
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: [_entry("ac1-node")])
    monkeypatch.setattr("fno.claims.roster.read_roster", lambda **_kw: _reading("working"))

    assert live_worked_node_ids() == {"ac1-node": ["bp-worker"]}


def test_ac5_edge_flips_when_worker_stops_without_waiting(monkeypatch):
    state = ["working"]
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: [_entry("ac1-node")])
    monkeypatch.setattr("fno.claims.roster.read_roster", lambda **_kw: _reading(state[0]))

    assert live_worked_node_ids() == {"ac1-node": ["bp-worker"]}
    state[0] = "killed"
    assert live_worked_node_ids() == {}


def test_ac8_edge_degrades_loudly_when_roster_is_unreadable(monkeypatch, capsys):
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: [_entry("ac1-node")])
    monkeypatch.setattr(
        "fno.claims.roster.read_roster",
        lambda **_kw: RosterReading(False, 0, {}, "roster timeout"),
    )

    assert live_worked_node_ids() == {}
    assert "worked overlay degraded: roster timeout" in capsys.readouterr().err
    with pytest.raises(RuntimeError, match="roster timeout"):
        live_worked_node_ids(strict=True)


def test_graph_corruption_is_not_an_empty_worked_answer(monkeypatch, capsys):
    from fno.graph.store import GraphCorruptError

    monkeypatch.setattr(
        "fno.graph.store.read_graph_strict",
        lambda *_a, **_kw: (_ for _ in ()).throw(GraphCorruptError("graph corrupt")),
    )

    assert live_worked_node_ids() == {}
    assert "worked overlay degraded: graph corrupt" in capsys.readouterr().err
    with pytest.raises(GraphCorruptError, match="graph corrupt"):
        live_worked_node_ids(strict=True)


def test_one_unmeasurable_row_skips_itself_per_node(monkeypatch):
    """A live row with no harness session id blocks its own node, never the
    whole measure: the other nodes' entries stay correct (x-ae54)."""
    entries = [_entry("x-a238"), _entry("x-6d3c")]
    reading = RosterReading(
        True, 0, {}, "", {}, 0, (),
        {"x-a238": ("bp-a238-king-brief",)},
    )
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: entries)
    monkeypatch.setattr("fno.claims.roster.read_roster", lambda **_kw: reading)

    assert live_worked_node_ids(strict=True) == {
        "x-a238": ["bp-a238-king-brief (unmeasurable: no harness session id)"],
    }


def test_node_attributed_worker_reads_worked_without_a_session_row(monkeypatch):
    """A registry worker whose graph session row was never written (the
    spawn-time skip) still reads as worked through the node fold."""
    entries = [{"id": "x-ae54", "status": "in_progress", "sessions": []}]
    reading = RosterReading(
        True, 1,
        {"x-ae54": [{"name": "t-ae54-worked-granularity", "state": "working",
                     "cwd": "/worktrees/x-ae54", "row_id": "01a08dab-7d3a"}]},
    )
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: entries)
    monkeypatch.setattr("fno.claims.roster.read_roster", lambda **_kw: reading)

    assert live_worked_node_ids(strict=True) == {
        "x-ae54": ["t-ae54-worked-granularity"],
    }


def test_finished_node_attributed_worker_frees_the_node(monkeypatch):
    """The node fold respects liveness: a stopped worker is not live work."""
    entries = [{"id": "x-ae54", "status": "in_progress", "sessions": []}]
    reading = RosterReading(
        True, 1,
        {"x-ae54": [{"name": "t-ae54-worked-granularity", "state": "killed",
                     "cwd": "/worktrees/x-ae54", "row_id": "01a08dab-7d3a"}]},
    )
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: entries)
    monkeypatch.setattr("fno.claims.roster.read_roster", lambda **_kw: reading)

    assert live_worked_node_ids(strict=True) == {}


def test_read_roster_folds_unmeasurable_pairs(monkeypatch):
    """The producer's structured advisory line lands on the reading as node
    attribution, not as a blocking refusal."""
    monkeypatch.setattr(
        "fno.agents.watchdog.fleet_rows",
        lambda **_kw: ([], [
            "roster advisory: unmeasurable-row: "
            "harness=codex node=x-a238 name=bp-a238-king-brief",
        ]),
    )

    from fno.claims.roster import read_roster

    reading = read_roster()

    assert reading.consulted is True
    assert reading.unmeasurable_by_node == {"x-a238": ["bp-a238-king-brief"]}


def test_all_terminal_graph_skips_the_roster_probe(monkeypatch):
    """No non-terminal node can be worked, so the fleet probe is wasted there
    and display paths keep their instant answer."""
    entries = [{"id": "done-1", "status": "done", "sessions": []}]

    def _boom(**_kw):
        raise AssertionError("roster probed on an all-terminal graph")

    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: entries)
    monkeypatch.setattr("fno.claims.roster.read_roster", _boom)

    assert live_worked_node_ids(strict=True) == {}
