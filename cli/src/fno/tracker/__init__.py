"""Work-item tracker package: the seam between footnote and a backlog store.

Consumers call :func:`get_tracker` rather than reading graph.json directly, so
the backend is selectable without touching call sites. The default and only
backend today is :class:`GraphTracker` (today's graph.json, unchanged in
shape); external backends (GitHub Issues, Linear, Jira) will register here as
they ship. graph.json stays the default forever: a stock install with no
account must work offline.
"""
from __future__ import annotations

from .graph_backend import GraphTracker
from .types import NodeNotFound, NodeTracker, TrackerError, TrackerNode, TrackerState

_BACKENDS = {"graph": GraphTracker}


def get_tracker(name: str | None = None) -> NodeTracker:
    """Return the configured work-item tracker.

    ``name`` selects a backend explicitly (used by tests and, eventually, by
    config-driven selection). The default is ``"graph"`` (graph.json), which is
    the offline stock-install backend and stays the default.
    """
    backend = name or "graph"
    cls = _BACKENDS.get(backend)
    if cls is None:
        raise ValueError(
            f"unknown tracker backend: {backend!r}. Available: {sorted(_BACKENDS)}"
        )
    return cls()


__all__ = [
    "GraphTracker",
    "NodeNotFound",
    "NodeTracker",
    "TrackerError",
    "TrackerNode",
    "TrackerState",
    "get_tracker",
]
