"""Work-item tracker package: the seam between footnote and a backlog store.

Consumers call :func:`get_tracker` rather than reading graph.json directly, so
the backend is selectable without touching call sites. Backend selection is
env-driven so it works with no config-schema machinery: ``FNO_TRACKER_BACKEND``
(default ``graph``). graph.json stays the default forever: a stock install with
no account must work offline.

Every backend answers in Rust (``crates/fno-agents/src/tracker/``), through
``fno-agents graph-get``'s stdin door; this module is the exec client.
"""
from __future__ import annotations

import os

from .types import (
    NodeNotFound,
    NodeTracker,
    TrackerCandidate,
    TrackerError,
    TrackerNode,
    TrackerState,
)


def active_backend_name(name: str | None = None) -> str:
    """The selected backend tag (``graph`` by default). Pure; no side effects.

    An explicit ``name`` wins (tests); otherwise ``FNO_TRACKER_BACKEND`` selects,
    defaulting to ``"graph"``. Shared with :func:`get_tracker` so the verb-refusal
    guard and backend construction cannot disagree on which backend is live.
    """
    return name or os.environ.get("FNO_TRACKER_BACKEND") or "graph"


def get_tracker(name: str | None = None) -> NodeTracker:
    """Return the configured work-item tracker.

    ``name`` selects a backend explicitly (used by tests). Otherwise the
    ``FNO_TRACKER_BACKEND`` env var selects, defaulting to ``"graph"``. An
    unknown backend now fails at the first call, with the Rust refusal text.
    """
    return _RustTracker(active_backend_name(name))


class _RustTracker:
    """Every backend answers in Rust, through `fno-agents graph-get`'s stdin door."""

    def __init__(self, name: str) -> None:
        self.name = name

    def _call(self, op: str, id: str | None = None) -> dict:
        from fno.rust_binary import verb_call

        out = verb_call(
            "graph-get", {"tracker": op, "backend": self.name, "id": id}, TrackerError, timeout=120
        )
        if out.get("not_found"):
            raise NodeNotFound(id)
        if out.get("error"):
            raise TrackerError(out["error"])
        return out

    def read(self, id: str) -> TrackerNode:
        return TrackerNode(**self._call("read", id)["node"])

    def list_open(self) -> list[TrackerCandidate]:
        return [TrackerCandidate(**c) for c in self._call("list-open")["candidates"]]

    def snapshot(self) -> dict:
        return self._call("snapshot")

    def close(self, id: str) -> None:
        self._call("close", id)


__all__ = [
    "NodeNotFound",
    "NodeTracker",
    "TrackerCandidate",
    "TrackerError",
    "TrackerNode",
    "TrackerState",
    "active_backend_name",
    "get_tracker",
]
