"""Full-text search lives in the store (task 16.1): ``fts.search`` is a
thin client for the keeper's ``search`` read op over the ``nodes_fts``
FTS5 index in graph.db. Results come from the store, no sidecar cache
file is ever written, and an unreachable keeper degrades ``find --fts``
to substring search with a warning.

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
    """Isolated state root for direct and native graph search reads."""
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


def test_find_fts_uses_native_index_when_available(tmp_graph, tmp_path, monkeypatch):
    """`find --fts` orders results through the native FTS5 index when present."""
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    (tmp_path / "config.toml").write_text(f'state_dir = "{tmp_path}"\n')
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "config.toml"))

    proc = _sp.run(
        [str(binary), "backlog", "find", "--fts", "resume handle", "-J"],
        capture_output=True,
        text=True,
        env={**_os.environ, "FNO_TRACKER_BACKEND": "graph"},
    )
    assert proc.returncode == 0, proc.stderr
    assert "warning: fts unavailable" not in proc.stderr
    assert [e["id"] for e in json.loads(proc.stdout)] == ["x-aaaa"]
