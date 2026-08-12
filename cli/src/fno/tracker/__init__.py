"""Work-item tracker package: the seam between footnote and a backlog store.

Consumers call :func:`get_tracker` rather than reading graph.json directly, so
the backend is selectable without touching call sites. The default backend is
graph.json (today's behaviour, unchanged); GitHub Issues is the first external
backend. graph.json stays the default forever: a stock install with no account
must work offline.

Backend selection is env-driven so it works with no config-schema machinery:
``FNO_TRACKER_BACKEND=github`` opts into GitHub Issues, and
``FNO_TRACKER_GITHUB_REPO=owner/repo`` scopes its ``list_open``. Default is
``graph``.
"""
from __future__ import annotations

import os

from .graph_backend import GraphTracker
from .github_backend import GitHubIssuesTracker
from .types import NodeNotFound, NodeTracker, TrackerError, TrackerNode, TrackerState


def get_tracker(name: str | None = None) -> NodeTracker:
    """Return the configured work-item tracker.

    ``name`` selects a backend explicitly (used by tests). Otherwise the
    ``FNO_TRACKER_BACKEND`` env var selects, defaulting to ``"graph"``.
    """
    backend = name or os.environ.get("FNO_TRACKER_BACKEND") or "graph"
    if backend == "graph":
        return GraphTracker()
    if backend == "github":
        return GitHubIssuesTracker(
            default_repo=os.environ.get("FNO_TRACKER_GITHUB_REPO")
        )
    raise ValueError(f"unknown tracker backend: {backend!r}. Available: graph, github")


__all__ = [
    "GitHubIssuesTracker",
    "GraphTracker",
    "NodeNotFound",
    "NodeTracker",
    "TrackerError",
    "TrackerNode",
    "TrackerState",
    "get_tracker",
]
