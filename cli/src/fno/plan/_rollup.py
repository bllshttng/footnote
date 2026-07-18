"""Compute epic progress rollup counters from the graph (x-6c2b wave 2).

An epic's plan doc carries glanceable counters describing its children, so "how
is this epic going" is a read, not a graph walk. Metadata only: counters
describe children, they never change scope (an epic stays non-dispatchable).

Wave 2 counts an epic's DIRECT children. Wave 3 folds a child-epic's leaves in
(mission -> epic -> leaf) so a mission aggregates across its epics.
"""
from __future__ import annotations

from typing import Any

# Derived `_status` buckets. Everything not named here (ready/idea/deferred/
# superseded) counts toward the total only.
_DONE = "done"
_IN_FLIGHT = frozenset({"claimed", "in_review"})
_BLOCKED = "blocked"

ROLLUP_KEYS: tuple[str, ...] = (
    "children_total",
    "children_done",
    "children_in_flight",
    "children_blocked",
    "progress",
)


def _direct_children(entries: list[dict[str, Any]], parent_id: str) -> list[dict]:
    return [
        n for n in entries
        if isinstance(n, dict) and n.get("parent") == parent_id
    ]


def compute_rollup(epic_id: str, entries: list[dict[str, Any]]) -> dict[str, Any]:
    """Return the rollup counters for ``epic_id`` from its DIRECT children.

    Each direct child counts once by its derived ``_status``. A childless epic
    returns zeros with ``progress: "0/0"`` - never a crash, never absent keys.
    """
    total = done = in_flight = blocked = 0
    for child in _direct_children(entries, epic_id):
        total += 1
        st = child.get("_status")
        if st == _DONE:
            done += 1
        elif st in _IN_FLIGHT:
            in_flight += 1
        elif st == _BLOCKED:
            blocked += 1
    return {
        "children_total": total,
        "children_done": done,
        "children_in_flight": in_flight,
        "children_blocked": blocked,
        "progress": f"{done}/{total}",
    }
