"""Rollup resolution: which mission (epic) does this feature serve?

Agent throughput makes feature-at-a-time cheap, so a backlog fills with
locally-good features that never compose into a mission. This module answers
"what epic does this serve?" at intake and keeps the answer visible afterward.

Two contracts hold everywhere this is used:

- **Metadata-only.** Rollup writes ``parent`` and ``orphan_ok`` and nothing
  else. It never gates, blocks, or reshapes a feature.
- **Soft signal.** An orphan is never refused. Visibility is the enforcement:
  a health metric, a board flag, and an in-band ordering tiebreaker.

``is_orphan`` is the single predicate behind all three of those surfaces, so
they cannot disagree about what counts.
"""
from __future__ import annotations

from typing import Any, NamedTuple, Optional

from fno.graph.relatedness import _RETIRED_EPIC_STATUSES, epic_candidates

Entry = dict[str, Any]

# Only these types roll up. Bugs, epics, and roadmap containers are exempt by
# type: a bug serves a defect, not a mission (AC6).
ROLLUP_TYPES = frozenset({"feature", "task"})

# Work that is over. A shipped feature with no mission edge is history, not a
# rollup an operator can still make.
CLOSED_STATUSES = frozenset({"done", "superseded", "deferred"})

# Auto-link bar. Deliberately high, and margin-gated so two plausible epics
# never coin-flip a parent edge - below either bar we suggest instead.
AUTO_LINK_MIN = 0.55
AUTO_LINK_MARGIN = 0.20


class Resolution(NamedTuple):
    """Outcome of the rollup ladder for one node.

    ``kind`` is ``exempt`` | ``linked`` | ``suggest`` | ``orphan``. ``epic_id``
    and ``score`` are set only for ``linked``; ``candidates`` carries the
    scored top-K for ``suggest``.
    """

    kind: str
    epic_id: Optional[str] = None
    score: float = 0.0
    candidates: tuple[tuple[str, float, str], ...] = ()
    reason: str = ""


def _id_index(entries: list[Entry]) -> dict[str, Entry]:
    return {
        e["id"]: e
        for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }


def has_epic_ancestor(entry: Entry, id_to_entry: dict[str, Entry]) -> bool:
    """True iff walking ``entry``'s parent chain reaches an epic.

    Walks the full chain rather than stopping at ``EPIC_NEST_MAX_DEPTH``: the
    cap bounds how deep epics may nest, not how deep a leaf may sit, and a node
    that does reach a mission must never be reported as an orphan. The ``seen``
    set bounds a malformed parent cycle.
    """
    seen: set[str] = set()
    current = entry.get("parent")
    while isinstance(current, str) and current not in seen:
        seen.add(current)
        parent = id_to_entry.get(current)
        if parent is None:
            return False
        if parent.get("type") == "epic":
            return True
        current = parent.get("parent")
    return False


def is_orphan(entry: Entry, id_to_entry: dict[str, Entry]) -> bool:
    """True iff this is OPEN feature/task work with no mission edge and no opt-out.

    Exempt nodes (wrong type, a deliberate ``orphan_ok``, or closed work) are
    False, so they stay invisible to the metric, the flag, and the tiebreaker
    alike (AC6).

    Closed work is excluded here rather than at each call site: a shipped
    feature is history, not a rollup an operator can still make, and scoping it
    per-surface is what let a Done card render an ``[orphan]`` tag the health
    metric had already excluded.
    """
    if not isinstance(entry, dict):
        return False
    if entry.get("type") not in ROLLUP_TYPES:
        return False
    if entry.get("orphan_ok"):
        return False
    if entry.get("status") in CLOSED_STATUSES:
        return False
    return not has_epic_ancestor(entry, id_to_entry)


def orphan_ids(entries: list[Entry]) -> frozenset[str]:
    """Ids of every orphan in ``entries``. Builds the id index once."""
    index = _id_index(entries)
    return frozenset(
        nid for nid, e in index.items() if is_orphan(e, index)
    )


def resolve(node: Entry, entries: list[Entry]) -> Resolution:
    """Run the rollup ladder for a node that already exists in ``entries``.

    Pure: scores and decides, never mutates. The caller applies a ``linked``
    result and prints the receipt, so the mutation stays on the locked path.
    """
    if node.get("type") not in ROLLUP_TYPES or node.get("orphan_ok"):
        return Resolution("exempt")
    # ANY explicit parent is the operator's answer to "what does this serve",
    # even one pointing at a plain feature rather than an epic. Rollup proposes
    # an edge where none exists; it never overrules one a human set, because the
    # printed undo (`--parent null`) could not restore what it overwrote.
    if node.get("parent"):
        return Resolution("exempt", reason="parent already set")

    candidates = tuple(epic_candidates(node, entries))
    if not candidates:
        # "This serves no mission" is only advice worth giving when missions
        # exist to serve. On a graph with no live epic there is nothing to link
        # to and nothing the operator can do, so the line would fire on every
        # single intake and carry no information. The health metric and the
        # board flag still count the node; only the intake line is suppressed.
        if not any(
            isinstance(e, dict)
            and e.get("type") == "epic"
            and e.get("status") not in _RETIRED_EPIC_STATUSES
            for e in entries
        ):
            return Resolution("exempt", reason="no epics in graph")
        return Resolution("orphan")

    top_id, top_score, top_reason = candidates[0]
    runner_up = candidates[1][1] if len(candidates) > 1 else 0.0
    if top_score >= AUTO_LINK_MIN and (top_score - runner_up) >= AUTO_LINK_MARGIN:
        return Resolution("linked", top_id, top_score, candidates, top_reason)
    return Resolution("suggest", candidates=candidates)


def receipt_lines(
    resolution: Resolution, node_id: str, id_to_entry: dict[str, Entry]
) -> list[str]:
    """Operator-facing lines for a resolution. Empty for ``exempt``."""

    def _title(eid: str) -> str:
        entry = id_to_entry.get(eid) or {}
        return str(entry.get("title") or eid)

    if resolution.kind == "linked":
        eid = resolution.epic_id or ""
        return [
            f'rollup: auto-linked {node_id} -> {eid} "{_title(eid)}" '
            f"(score {resolution.score:.2f}); "
            f"undo: fno backlog update {node_id} --parent null",
        ]
    if resolution.kind == "suggest":
        lines = [f"rollup: no clear mission edge for {node_id}; candidates:"]
        for eid, score, _reason in resolution.candidates:
            lines.append(
                f'  {score:.2f}  {eid}  "{_title(eid)}"  '
                f"-> fno backlog update {node_id} --parent {eid}"
            )
        return lines
    if resolution.kind == "orphan":
        return [
            f"rollup: no mission edge (orphan); mark deliberate with: "
            f'fno backlog update {node_id} --orphan-ok "<reason>"',
        ]
    return []
