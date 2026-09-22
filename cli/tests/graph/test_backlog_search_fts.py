"""Full-text search lives in the store (task 16.1): ``fts.search`` is a
thin client for the keeper's ``search`` read op over the ``nodes_fts``
FTS5 index in graph.db. Results come from the store, no sidecar cache
file is ever written, and an unreachable keeper degrades ``find --fts``
to substring search with a warning.

Filter: ``fno doctor test cli/tests/graph/test_backlog_search_fts.py``
"""

from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.graph import fts
from fno.graph.cli import cli

runner = CliRunner()

FULL = {
    "type": "feature",
    "status": "ready",
    "priority": "p2",
    "domain": "code",
    "created_at": "2026-09-11T00:00:00+00:00",
}


def _row(node_id: str, **overrides):
    return {
        **FULL,
        "id": node_id,
        "slug": node_id,
        "title": node_id,
        "tags": [],
        **overrides,
    }


def _seed(graph, *rows) -> None:
    graph.write_text(json.dumps({"entries": list(rows)}), encoding="utf-8")


@pytest.fixture()
def tmp_graph(tmp_path, monkeypatch):
    """A temp machine with a real keeper. Skips where no worker binary."""
    from fno.graph.store import _worker_binary

    graph = tmp_path / "graph.json"
    _seed(
        graph,
        _row("x-aaaa", title="resume handle provenance join", description="the ledger stores session uuids"),
        _row("x-bbbb", title="unrelated work item"),
    )
    # two seams: the verbs mutate via _graph_path; `find`'s display reader
    # resolves through paths.graph_json (both documented test redirects)
    from fno.graph import cli as graph_cli

    monkeypatch.setattr(graph_cli, "_graph_path", lambda: graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    return graph


def test_search_answers_from_the_store_index(tmp_graph):
    hits = fts.search("resume handle", tmp_graph, limit=None)
    assert hits == ["x-aaaa"]
    assert list(tmp_graph.parent.glob("*.fts5")) == [], (
        "no sidecar cache file is ever written"
    )


def test_query_syntax_is_neutralized_end_to_end(tmp_graph):
    assert fts.search('"unbalanced quote AND (', tmp_graph) == []


def test_find_fts_flag_ranks_and_falls_back(tmp_graph, monkeypatch):
    r = runner.invoke(cli, ["find", "--fts", "resume handle", "-J"])
    assert r.exit_code == 0, r.output
    ids = [e["id"] for e in json.loads(r.output)]
    assert ids == ["x-aaaa"]

    # an fts failure degrades to the substring lane with a warning, not an exit
    def _boom(*a, **k):
        raise fts.SearchUnavailableError("no keeper here")

    monkeypatch.setattr(fts, "search", _boom)
    r2 = runner.invoke(cli, ["find", "--fts", "resume handle", "-J"])
    assert r2.exit_code == 0, r2.output
    assert "warning: fts unavailable" in r2.output
    # stderr warning and stdout JSON share one captured stream here
    body = r2.output[r2.output.index("[") :]
    assert [e["id"] for e in json.loads(body)] == ["x-aaaa"]
