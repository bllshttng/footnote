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


# ---------------------------------------------------------------------------
# Scope growth (x-d157): how much did an epic grow after it was decomposed?
# ---------------------------------------------------------------------------

# Below this fraction of the epic's window carrying an origin, the growth figure
# is suppressed rather than printed. At the capture rate this feature shipped
# against (27%), a low number is indistinguishable from poor capture, and that
# ambiguity flatters the process in the direction nobody would question. A
# constant, not config: the right level is a judgment about the data, and a
# per-project knob would let a low-capture project tune the check away.
SCOPE_GROWTH_COVERAGE_FLOOR = 0.50


class ScopeGrowth(NamedTuple):
    """Follow-up work an epic accumulated after decomposition, with its evidence.

    ``reportable`` is false when capture coverage sits below the floor. Callers
    must suppress ``follow_up_ids`` counting in that case and say why: a growth
    number without its coverage is not a measurement.

    ``realized_nodes`` / ``realized_prs`` are the ground-truth join - what the
    epic actually cost - against ``declared_size``, so the growth figure can be
    falsified. An epic reporting near-zero growth that shipped far past its size
    is evidence against the capture, not evidence about the epic.
    """

    epic_id: str
    follow_up_ids: tuple[str, ...]
    window_total: int
    window_with_origin: int
    coverage: float
    reportable: bool
    realized_nodes: int
    realized_prs: int
    declared_size: Optional[str]


def origin_index(entries: list[Entry]) -> dict[str, list[Entry]]:
    """``source_node_id`` inverted: origin id -> the nodes that name it."""
    out: dict[str, list[Entry]] = {}
    for e in entries:
        if not isinstance(e, dict):
            continue
        src = e.get("source_node_id")
        if src:
            out.setdefault(src, []).append(e)
    return out


def _parent_descendants(entries: list[Entry], root_id: str) -> set:
    """Every node under ``root_id`` by ``parent``, transitively."""
    by_parent: dict[str, list[str]] = {}
    for e in entries:
        if isinstance(e, dict) and e.get("parent"):
            by_parent.setdefault(e["parent"], []).append(e["id"])
    out: set = set()
    frontier = [root_id]
    while frontier:
        current = frontier.pop()
        for child_id in by_parent.get(current, []):
            if child_id not in out:
                out.add(child_id)
                frontier.append(child_id)
    return out


def scope_growth(
    entries: list[Entry],
    epic_id: str,
    *,
    floor: float = SCOPE_GROWTH_COVERAGE_FLOOR,
) -> ScopeGrowth:
    """Work the epic grew after decomposition, plus the coverage that qualifies it.

    The follow-up set is every node reachable from the epic's descendants by
    ``source_node_id``, minus the descendants themselves: work that came out of
    the epic without being planned into it. A node already under the epic by
    ``parent`` was decomposed in, not grown.

    Coverage is measured over the epic's window - nodes created at or after it -
    because that is the population in which a follow-up could have been
    captured. Nodes older than the epic could not have named it.
    """
    descendants = _parent_descendants(entries, epic_id)
    by_origin = origin_index(entries)

    follow_ups: set = set()
    frontier = list(descendants)
    seen = set(descendants) | {epic_id}
    while frontier:
        for child in by_origin.get(frontier.pop(), []):
            child_id = child.get("id")
            if child_id in seen:
                continue
            seen.add(child_id)
            follow_ups.add(child_id)
            frontier.append(child_id)

    epic = _id_index(entries).get(epic_id, {})
    epic_born = epic.get("created_at") or ""
    window = [
        e for e in entries
        if isinstance(e, dict)
        and e.get("id") != epic_id
        and (e.get("created_at") or "") >= epic_born
    ]
    with_origin = sum(1 for e in window if e.get("source_node_id"))
    coverage = (with_origin / len(window)) if window else 0.0

    realized = [e for e in entries if isinstance(e, dict) and e.get("id") in descendants]
    return ScopeGrowth(
        epic_id=epic_id,
        follow_up_ids=tuple(sorted(follow_ups)),
        window_total=len(window),
        window_with_origin=with_origin,
        coverage=coverage,
        reportable=coverage >= floor,
        realized_nodes=len(realized),
        realized_prs=len({e["pr_number"] for e in realized if e.get("pr_number")}),
        declared_size=epic.get("size"),
    )
