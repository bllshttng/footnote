"""Full-text search lives in the store, with no sidecar cache file.

The Python reader tests cover keeper lookup and query neutralization. The
native ``backlog find --fts`` test covers the compiled store index.

Filter: ``fno doctor test cli/tests/graph/test_backlog_search_fts.py``
"""

from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json

import pytest
from fno.graph import fts

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
    seed_graph(graph, json.dumps({"entries": list(rows)}))


@pytest.fixture()
def tmp_graph(tmp_path, monkeypatch):
    """A temporary graph store rooted under the test directory."""
    graph = tmp_path / "graph.json"
    _seed(
        graph,
        _row("x-aaaa", title="resume handle provenance join", description="the ledger stores session uuids"),
        _row("x-bbbb", title="unrelated work item"),
    )
    return graph


def test_search_answers_from_the_store_index(tmp_graph):
    hits = fts.search("resume handle", tmp_graph, limit=None)
    assert hits == ["x-aaaa"]
    assert list(tmp_graph.parent.glob("*.fts5")) == [], (
        "no sidecar cache file is ever written"
    )


def test_query_syntax_is_neutralized_end_to_end(tmp_graph):
    assert fts.search('"unbalanced quote AND (', tmp_graph) == []


def test_find_fts_uses_the_native_index_without_degrade_warning(tmp_graph, tmp_path, monkeypatch):
    """The native `backlog find --fts` path answers from the store index."""
    from tests._native_door import run_native

    (tmp_path / "config.toml").write_text(f'state_dir = "{tmp_path}"\n')
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "config.toml"))

    code, out, err = run_native("backlog", "find", "--fts", "resume handle", "-J")
    assert code == 0, err
    assert "warning: fts unavailable" not in err
    assert [e["id"] for e in json.loads(out)] == ["x-aaaa"]
