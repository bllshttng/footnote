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


def compute_waves(
    epic_id: str, entries: list[dict[str, Any]]
) -> tuple[dict[str, int], int]:
    """Derive topological wave strata for an epic's DIRECT children (x-6c2b AC4).

    A child with no INTRA-epic blocker is wave 0; otherwise its wave is
    ``1 + max(wave of its intra-epic blockers)`` (longest-path strata). Only
    ``blocked_by`` edges between siblings of the same epic count - a child blocked
    solely by an external node is wave 0 within its epic. Edges are the sole
    ordering authority, so this is a pure view that cannot drift.

    Returns ``(wave_by_child_id, max_wave)`` where ``max_wave`` is -1 for a
    childless epic (so the caller's ``waves`` summary is ``max_wave + 1`` == 0).
    Cycle-safe: a blocked_by cycle among siblings resolves those nodes to wave 0
    rather than recursing forever.
    """
    children = _direct_children(entries, epic_id)
    child_ids = {c["id"] for c in children if c.get("id")}
    blockers: dict[str, list[str]] = {}
    for c in children:
        cid = c.get("id")
        if not cid:
            continue
        blockers[cid] = [
            b for b in (c.get("blocked_by") or []) if b in child_ids
        ]

    # Kahn-style leveling: assign a node its wave only once ALL its intra-epic
    # blockers have waves, so the result is deterministic (independent of dict
    # order) and a longest-path stratification. When the fixpoint stalls, only
    # the nodes ON a cycle collapse to wave 0; an acyclic dependent of a cycle
    # (C blocked by cyclic A) is left to restratify on the next round, so it
    # still lands at `1 + max(blocker wave)` rather than being flattened.
    wave: dict[str, int] = {}
    remaining = set(child_ids)
    while remaining:
        progressed = False
        for cid in list(remaining):
            bl = blockers[cid]
            if all(b in wave for b in bl):
                wave[cid] = 1 + max((wave[b] for b in bl), default=-1)
                remaining.discard(cid)
                progressed = True
        if progressed:
            continue
        # Stalled: break the knot. Collapse only the true cycle members (a node
        # that can reach itself over unresolved blockers) to wave 0, then loop -
        # their acyclic dependents resolve normally against those zeros.
        cyclic = {c for c in remaining if _on_cycle(c, blockers, remaining)}
        if not cyclic:  # defensive: no cycle but still stuck -> flush to 0
            cyclic = set(remaining)
        for c in cyclic:
            wave[c] = 0
            remaining.discard(c)

    max_wave = max(wave.values()) if wave else -1
    return wave, max_wave


def _on_cycle(
    start: str, blockers: dict[str, list[str]], scope: set[str]
) -> bool:
    """True iff ``start`` can reach itself over unresolved blocker edges within
    ``scope`` - i.e. it is a member of a blocked_by cycle, not merely a dependent
    of one. DFS bounded by ``scope`` so it always terminates."""
    stack = [b for b in blockers.get(start, []) if b in scope]
    seen: set[str] = set()
    while stack:
        cur = stack.pop()
        if cur == start:
            return True
        if cur in seen:
            continue
        seen.add(cur)
        stack.extend(b for b in blockers.get(cur, []) if b in scope)
    return False


def compute_rollup(
    epic_id: str,
    entries: list[dict[str, Any]],
    _seen: frozenset[str] | None = None,
) -> dict[str, Any]:
    """Return the LEAF rollup counters for ``epic_id`` (x-6c2b wave 3).

    A direct leaf child counts once by its derived ``_status``; a direct child
    that is itself an epic recurses ONE level and folds its leaves in (so a
    mission aggregates its epics' leaves, never counting an epic as a unit).
    Depth caps at mission -> epic -> leaf; the ``_seen`` guard bounds recursion
    so a malformed graph with an epic-parent cycle terminates instead of
    looping. A childless epic returns zeros with ``progress: "0/0"``.
    """
    seen = _seen or frozenset()
    if epic_id in seen:
        return {
            "children_total": 0, "children_done": 0,
            "children_in_flight": 0, "children_blocked": 0, "progress": "0/0",
        }
    seen = seen | {epic_id}

    total = done = in_flight = blocked = 0
    for child in _direct_children(entries, epic_id):
        if child.get("type") == "epic":
            cid = child.get("id")
            if not cid:
                # An id-less epic child would recurse on None and fold in every
                # top-level node (parent == None). Skip it rather than miscount.
                continue
            sub = compute_rollup(cid, entries, seen)
            total += sub["children_total"]
            done += sub["children_done"]
            in_flight += sub["children_in_flight"]
            blocked += sub["children_blocked"]
            continue
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
