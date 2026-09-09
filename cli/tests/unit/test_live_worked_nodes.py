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
    monkeypatch.setattr("fno.graph.store.read_graph", lambda *_a, **_kw: [_entry("ac1-node")])
    monkeypatch.setattr("fno.claims.roster.read_roster", lambda **_kw: _reading("working"))

    assert live_worked_node_ids() == {"ac1-node": ["bp-worker"]}


def test_ac5_edge_flips_when_worker_stops_without_waiting(monkeypatch):
    state = ["working"]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda *_a, **_kw: [_entry("ac1-node")])
    monkeypatch.setattr("fno.claims.roster.read_roster", lambda **_kw: _reading(state[0]))

    assert live_worked_node_ids() == {"ac1-node": ["bp-worker"]}
    state[0] = "killed"
    assert live_worked_node_ids() == {}


def test_ac8_edge_degrades_loudly_when_roster_is_unreadable(monkeypatch, capsys):
    monkeypatch.setattr("fno.graph.store.read_graph", lambda *_a, **_kw: [_entry("ac1-node")])
    monkeypatch.setattr(
        "fno.claims.roster.read_roster",
        lambda **_kw: RosterReading(False, 0, {}, "roster timeout"),
    )

    assert live_worked_node_ids() == {}
    assert "worked overlay degraded: roster timeout" in capsys.readouterr().err
    with pytest.raises(RuntimeError, match="roster timeout"):
        live_worked_node_ids(strict=True)
