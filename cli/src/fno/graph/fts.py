"""Full-text search over the backlog, served by the keeper.

The FTS5 index (``nodes_fts``) lives in graph.db beside the nodes table,
kept in step by triggers (crates/fno-agents/src/backlog/search.rs); this
module is a thin client for the keeper's ``search`` read op. When the
keeper is unreachable, ``search`` raises :class:`SearchUnavailableError`
and callers degrade to substring search.
"""
from __future__ import annotations

from pathlib import Path

from fno.graph.store import _client_for


class SearchUnavailableError(RuntimeError):
    """The keeper, and with it the store's FTS index, is unreachable."""


def search(query: str, graph_path: Path, limit: int | None = 20) -> list[str]:
    """Node ids matching ``query``, store-ranked (BM25)."""
    try:
        result = _client_for(Path(graph_path)).request(
            "api", {"op": "search", "q": query, "limit": limit}
        )
    except FileNotFoundError:
        raise
    except RuntimeError as exc:
        raise SearchUnavailableError(str(exc)) from exc
    return [str(row) for row in result.get("rows", [])]
