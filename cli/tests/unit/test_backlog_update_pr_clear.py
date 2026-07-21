"""`backlog update --pr-number/--pr-url null` clears the link.

Every other nullable scalar on this verb documents `'null' clears`; these two
did not honor it, and the graph is hand-edit-forbidden - so a node linked to the
wrong PR could not be unlinked at all, and the mislink would ride to a merge and
close a node that shipped nothing.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _first(g: Path) -> dict:
    return json.loads(g.read_text())["entries"][0]


def test_pr_link_can_be_cleared(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_number": 504, "pr_url": "https://example.com/pull/504",
    }])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-number", "null", "--pr-url", "null",
    ])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] is None
    assert node["pr_url"] is None


def test_pr_number_still_sets_an_int(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "77", "--pr-url", "https://github.com/o/r/pull/77",
    ])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] == 77
    assert node["pr_url"] == "https://github.com/o/r/pull/77"


def test_pr_number_alone_derives_the_url(tmp_graph, monkeypatch):
    """A url-less pr_number names no repo, and PR numbers collide across repos."""
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: f"https://github.com/o/r/pull/{pr}")
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-number", "77"])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["pr_url"] == "https://github.com/o/r/pull/77"


def test_pr_number_refused_when_repo_unresolvable(tmp_graph, monkeypatch):
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: None)
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-number", "77"])

    assert result.exit_code != 0
    # Both remedies must be named: naming only one leaves the caller stuck.
    assert "gh auth login" in result.output and "--pr-url" in result.output
    assert _first(tmp_graph).get("pr_number") is None


def test_unparseable_pr_url_is_rejected(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-number", "77", "--pr-url", "not-a-url",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_url") is None


def test_clearing_the_url_while_setting_a_number_is_refused(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-number", "77", "--pr-url", "null",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_number") is None


def test_plan_path_can_be_cleared(tmp_graph):
    """A literal 'null' would bind the node to a plan file named "null", which
    reads as bound to every gate that only checks presence."""
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "plan_path": "/plans/old.md",
    }])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", "null",
    ])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["plan_path"] is None


def test_plan_path_still_binds_a_real_path(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", "/plans/new.md",
    ])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["plan_path"] == "/plans/new.md"


def test_clearing_the_url_alone_is_refused_when_a_number_remains(tmp_graph):
    """Clearing only the url strands the pr_number the node already carries."""
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_number": 77, "pr_url": "https://github.com/o/r/pull/77",
    }])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-url", "null"])

    assert result.exit_code != 0
    node = _first(tmp_graph)
    assert node["pr_number"] == 77
    assert node["pr_url"] == "https://github.com/o/r/pull/77"


def test_clearing_the_url_alone_is_fine_when_no_number_remains(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_url": "https://github.com/o/r/pull/77",
    }])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-url", "null"])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["pr_url"] is None


def test_unparseable_pr_url_rejected_without_a_pr_number(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-url", "not-a-url"])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_url") is None


def test_add_pr_derives_its_url(tmp_graph, monkeypatch):
    """additional_prs entries are read by the same repo-scoped matcher, so a
    bare --add-pr is unattributable for the same reason a bare --pr-number is."""
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: f"https://github.com/o/r/pull/{pr}")
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--add-pr", "88"])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["additional_prs"] == [
        {"number": 88, "url": "https://github.com/o/r/pull/88"}
    ]


def test_add_pr_refused_when_repo_unresolvable(tmp_graph, monkeypatch):
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: None)
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--add-pr", "88"])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("additional_prs") in (None, [])
