"""Tests for the hidden ``fno backlog worked`` authority verb."""
from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.graph.cli import cli


runner = CliRunner()


def _entry(node_id: str = "ac1-node") -> dict:
    return {
        "id": node_id,
        "status": "ready",
        "sessions": [
            {
                "phase": "blueprint",
                "harness": "claude",
                "session_id": "session-1",
                "started_at": "2026-09-09T00:00:00Z",
            }
        ],
    }


def test_ac1_hp_worked_json_names_worker(monkeypatch):
    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", lambda **_kw: {"ac1-node": ["bp-worker"]})
    monkeypatch.setattr("fno.graph.store.read_graph", lambda *_a, **_kw: [_entry()])

    result = runner.invoke(cli, ["worked", "--json"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout) == [
        {
            "id": "ac1-node",
            "status": "ready",
            "workers": ["bp-worker"],
            "phases": ["blueprint"],
        }
    ]


def test_ac1_hp_worked_text_is_one_line_per_node(monkeypatch):
    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", lambda **_kw: {"ac1-node": ["bp-worker"]})
    monkeypatch.setattr("fno.graph.store.read_graph", lambda *_a, **_kw: [_entry()])

    result = runner.invoke(cli, ["worked"])

    assert result.exit_code == 0, result.output
    assert result.stdout == "ac1-node  ready  bp-worker\n"


def test_ac8_edge_worked_refuses_when_authority_unavailable(monkeypatch):
    def _raise(**_kw):
        raise RuntimeError("roster timeout")

    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", _raise)

    result = runner.invoke(cli, ["worked", "--json"])

    assert result.exit_code == 1
    assert "roster timeout" in result.output
