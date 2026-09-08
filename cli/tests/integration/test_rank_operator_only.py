"""Rank is the operator's pin; an agent session votes instead (AC1)."""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.harness_identity import FNO_HARNESS_NAME, FNO_HARNESS_SESSION_ID

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}))


def _rank_of(g: Path, node_id: str):
    for e in json.loads(g.read_text())["entries"]:
        if e.get("id") == node_id:
            return e.get("rank")
    return None


def _agent_session(monkeypatch) -> None:
    monkeypatch.setenv(FNO_HARNESS_NAME, "claude")
    monkeypatch.setenv(FNO_HARNESS_SESSION_ID, "sess-agent-1")


LOOSE = [
    {"id": "x-aaa1", "title": "First", "status": "ready", "priority": "p1",
     "project": "fno"},
    {"id": "x-bbb2", "title": "Second", "status": "ready", "priority": "p1",
     "project": "fno", "rank": -4.0},
]


def test_ac1_hp_agent_rank_top_refused(tmp_graph, monkeypatch):
    """An agent session is refused, writes no rank, and is told how to vote."""
    _seed(tmp_graph, LOOSE)
    _agent_session(monkeypatch)

    result = runner.invoke(app, ["backlog", "rank", "x-aaa1", "--top"])

    assert result.exit_code != 0
    assert _rank_of(tmp_graph, "x-aaa1") is None
    assert "operator-only" in result.output
    assert "fno backlog encounter x-aaa1 --evidence" in result.output
    assert "fno backlog update x-aaa1 --priority" in result.output


def test_ac1_hp_refusal_covers_every_write_action(tmp_graph, monkeypatch):
    """--bottom, --after and --clear are rank writes too, so all are refused."""
    _seed(tmp_graph, LOOSE)
    _agent_session(monkeypatch)

    for args in (
        ["--bottom"],
        ["--after", "x-bbb2"],
        ["--clear"],
    ):
        result = runner.invoke(app, ["backlog", "rank", "x-aaa1", *args])
        assert result.exit_code != 0, args
        assert "operator-only" in result.output, args
    assert _rank_of(tmp_graph, "x-aaa1") is None
    assert _rank_of(tmp_graph, "x-bbb2") == -4.0


def test_ac1_hp_partial_harness_stamp_still_refused(tmp_graph, monkeypatch):
    """A half-stamped session is an agent: the fence fails closed."""
    _seed(tmp_graph, LOOSE)
    monkeypatch.setenv(FNO_HARNESS_SESSION_ID, "sess-agent-1")
    monkeypatch.delenv(FNO_HARNESS_NAME, raising=False)

    result = runner.invoke(app, ["backlog", "rank", "x-aaa1", "--top"])

    assert result.exit_code != 0
    assert _rank_of(tmp_graph, "x-aaa1") is None


def test_ac1_hp_a_harness_fno_never_spawned_is_still_refused(tmp_graph, monkeypatch):
    """The fno stamp is not the only prover: ancestry proves an ambient marker.

    A claude or codex session started by hand carries its own markers and no
    fno stamp, and it is an agent session all the same.
    """
    from fno.graph.rank import agent_harness_writing_rank

    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda env=None, **kw: SimpleNamespace(
            harness="claude", session_id="sess-ambient", disposition="owned"
        ),
    )

    assert agent_harness_writing_rank() == "claude"
    result = runner.invoke(app, ["backlog", "rank", "x-aaa1", "--top"])
    assert result.exit_code != 0
    assert _rank_of(tmp_graph, "x-aaa1") is None


def test_ac1_edge_operator_flag_writes_the_min_anchored_pin(tmp_graph, monkeypatch):
    """--operator from an agent shell pins exactly as before: min minus one."""
    _seed(tmp_graph, LOOSE)
    _agent_session(monkeypatch)

    result = runner.invoke(app, ["backlog", "rank", "x-aaa1", "--top", "--operator"])

    assert result.exit_code == 0, result.output
    assert _rank_of(tmp_graph, "x-aaa1") == -5.0


def test_ac1_edge_operator_shell_writes_without_the_flag(tmp_graph):
    """No harness stamp is the operator's own shell; the pin writes as today."""
    _seed(tmp_graph, LOOSE)

    result = runner.invoke(app, ["backlog", "rank", "x-aaa1", "--top"])

    assert result.exit_code == 0, result.output
    assert _rank_of(tmp_graph, "x-aaa1") == -5.0


def test_ac1_edge_receipt_names_the_lane_it_ordered_within(tmp_graph):
    """A loose node's receipt says which lane it is top OF, not a bare --top."""
    _seed(tmp_graph, LOOSE)

    result = runner.invoke(app, ["backlog", "rank", "x-aaa1", "--top"])

    assert result.exit_code == 0, result.output
    assert "--top of lane " in result.output
    assert "orders it within that board lane only" in result.output


def test_ac1_edge_receipt_names_the_epic_it_ordered_within(tmp_graph):
    """A child's receipt says it ranked inside its epic, not across the project."""
    _seed(tmp_graph, [
        {"id": "x-ea11", "title": "Epic", "status": "in_progress", "type": "epic",
         "priority": "p1", "project": "fno"},
        {"id": "x-cd01", "title": "Child one", "status": "ready", "priority": "p1",
         "project": "fno", "parent": "x-ea11"},
        {"id": "x-cd02", "title": "Child two", "status": "in_progress",
         "priority": "p1", "project": "fno", "parent": "x-ea11", "rank": -2.0},
    ])

    result = runner.invoke(app, ["backlog", "rank", "x-cd01", "--top"])

    assert result.exit_code == 0, result.output
    assert "--top of epic x-ea11" in result.output
    assert "orders it among that epic's children only" in result.output
    assert _rank_of(tmp_graph, "x-cd01") == -3.0
