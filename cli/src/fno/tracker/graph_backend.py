"""The default NodeTracker backend: today's graph.json, unchanged in shape.

graph.json is already "a sidecar plus a tracker merged into one record", so
the default backend is a projection, not a rewrite. read() narrows an entry to
the five-field interface; close() sets ``completed_at`` and lets the store's
``recompute_statuses`` derive ``done`` exactly as the ``backlog done`` verb
does. status is never written here: it is derived, and writing it would be
dropped by ``Entry._check_status_drift`` on the next read.
"""
from __future__ import annotations

import datetime as _dt
from pathlib import Path

from fno.graph.store import locked_mutate_graph, read_graph
from fno.graph.types import _derive_status
from fno.paths import graph_json

from .types import NodeNotFound, TrackerNode, TrackerState


def _ts_now() -> str:
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# A node is closed from the tracker's perspective once footnote itself regards
# it as terminal. ``done`` (work shipped) and ``superseded`` (replaced by
# another node) are the two terminal rungs; every other rung is still open and
# may be dispatched.
_CLOSED_RUNGS = frozenset({"done", "superseded"})


class GraphTracker:
    """NodeTracker over graph.json. ``name`` is the stable backend tag."""

    name = "graph"

    def __init__(self, path: Path | None = None) -> None:
        self._path = path or graph_json()

    def read(self, id: str) -> TrackerNode:
        for entry in read_graph(self._path):
            if entry.get("id") == id:
                return self._project(entry)
        raise NodeNotFound(id)

    def list_open(self) -> list[TrackerNode]:
        # Open == not yet terminal. Project once (which derives the rung) and
        # filter on the projected state, so each entry pays one derivation.
        # The backend returns the set in storage order; footnote's caller ranks it.
        return [
            node
            for node in (self._project(e) for e in read_graph(self._path))
            if node.state is TrackerState.open
        ]

    def close(self, id: str) -> None:
        ts = _ts_now()

        def _mark_done(entries: list[dict]) -> list[dict]:
            for entry in entries:
                if entry.get("id") == id:
                    # completed_at is the real closure signal; recompute_statuses
                    # (run by locked_mutate_graph) derives status=done from it.
                    entry["completed_at"] = ts
                    break
            else:
                raise NodeNotFound(id)
            return entries

        locked_mutate_graph(self._path, _mark_done)

    @staticmethod
    def _project(entry: dict) -> TrackerNode:
        rung = _derive_status(entry)
        return TrackerNode(
            id=entry["id"],
            title=entry.get("title"),
            state=(
                TrackerState.closed
                if rung in _CLOSED_RUNGS
                else TrackerState.open
            ),
            parent=entry.get("parent"),
            blocked_by=list(entry.get("blocked_by") or []),
        )
