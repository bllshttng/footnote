"""Acceptance tests for difficulty on every graph-node birth path."""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    graph = tmp_path / "graph.json"
    graph.write_text('{"entries": []}\n')
    import fno.graph._constants as constants
    import fno.graph.store as store

    monkeypatch.setattr(constants, "GRAPH_JSON", graph)
    monkeypatch.setattr(constants, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(constants, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(constants, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(store, "GRAPH_JSON", graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    return graph


def test_backlog_builder_requires_an_explicit_difficulty_decision():
    """Omitting difficulty from the shared builder must fail at the call site."""
    from fno.graph.cli import _build_backlog_node

    with pytest.raises(TypeError):
        _build_backlog_node(title="missing decision")


def test_backlog_builder_accepts_explicit_none():
    """A caller may deliberately preserve the optional null band."""
    from fno.graph.cli import _build_backlog_node

    node = _build_backlog_node(title="explicitly unbanded", difficulty=None)

    assert node["difficulty"] is None


def test_backlog_add_requires_difficulty_noninteractive(tmp_graph):
    result = runner.invoke(app, ["backlog", "add", "missing difficulty"])

    assert result.exit_code == 2
    assert "non-interactive filing requires --difficulty" in result.output
    assert json.loads(tmp_graph.read_text())["entries"] == []


def test_backlog_new_writes_difficulty(tmp_graph):
    result = runner.invoke(
        app, ["backlog", "new", "mail-born work", "--difficulty", "high"]
    )

    assert result.exit_code == 0, result.output
    row = json.loads(tmp_graph.read_text())["entries"][0]
    assert row["difficulty"] == "high"
    assert row["difficulty_history"][-1]["source"] == "filed"


def test_capture_promote_requires_and_writes_difficulty(tmp_graph, tmp_path, monkeypatch):
    from fno.backlog.capture import add_item

    inbox = tmp_path / "inbox.md"
    monkeypatch.setattr("fno.backlog.capture._inbox_path", lambda: inbox)
    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: tmp_graph)
    item = add_item(
        inbox,
        title="promoted work",
        source="PR#1",
        why="a reason",
        where="a file",
        priority="p1",
    )

    refused = runner.invoke(app, ["backlog", "capture", "promote", item["id"]])
    assert refused.exit_code == 2
    assert "non-interactive filing requires --difficulty" in refused.output

    accepted = runner.invoke(
        app,
        ["backlog", "capture", "promote", item["id"], "--difficulty", "medium"],
    )
    assert accepted.exit_code == 0, accepted.output
    row = json.loads(tmp_graph.read_text())["entries"][0]
    assert row["difficulty"] == "medium"


def test_retro_severity_maps_to_difficulty():
    from fno.retro.classify import severity_to_difficulty

    assert severity_to_difficulty("critical") == "high"
    assert severity_to_difficulty("high") == "high"
    assert severity_to_difficulty("medium") == "medium"
    assert severity_to_difficulty("low") == "low"
    assert severity_to_difficulty(None) == "medium"


@pytest.mark.parametrize(
    ("severity", "expected"),
    [("high", "high"), (None, "medium")],
)
def test_retro_lander_threads_named_difficulty(severity, expected, tmp_path):
    from fno.retro.land import land_candidates
    from fno.retro.types import Candidate, TIER_NODE

    seen = []

    def create(**kwargs):
        seen.append(kwargs)
        return "ab-created"

    candidate = Candidate(
        title="retro work",
        body="body",
        tier=TIER_NODE,
        priority="p1",
        source_pr=1,
        source_id="comment-1",
        extra={"severity": severity},
    )
    land_candidates(
        [candidate],
        mode="autonomous",
        repo_root=tmp_path,
        create_fn=create,
        inbox_fn=lambda _candidate: None,
    )

    assert seen[0]["difficulty"] == expected


def test_retro_default_create_attributes_history_to_retro(tmp_graph, tmp_path, monkeypatch):
    import fno.graph.cli as graph_cli
    from fno.retro.land import _default_create

    monkeypatch.setattr(graph_cli, "_graph_path", lambda: tmp_graph)
    node_id = _default_create(
        title="retro work",
        details="body",
        priority="p1",
        difficulty="high",
        project="fno",
        cwd=str(tmp_path),
    )

    row = next(
        row for row in json.loads(tmp_graph.read_text())["entries"] if row["id"] == node_id
    )
    assert row["difficulty"] == "high"
    assert row["difficulty_history"][-1]["source"] == "retro"


def test_no_difficulty_dispatch_receipt_is_named():
    from fno.route_resolve import resolve_dispatch_model

    model, source, chain = resolve_dispatch_model()

    assert model is None
    assert source == "provider-default(no-difficulty)"
    assert chain == ["provider-default(no-difficulty)"]
