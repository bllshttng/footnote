"""The single owner of "has this worker's node already shipped" (x-1379).

A king reads ``fno agents top``, sees a provider lane at its cap, and reports
the next node parked on capacity - while the lane is held by workers whose
nodes already merged. Both halves of that fact existed; nothing joined them.
This module is the join, and every renderer imports it rather than growing a
second spelling of the rule.

The verdict is fail-closed on every read it cannot trust: an absence of
reported doneness is not doneness, because a human decides whether to kill a
session from this verdict. Doneness is :func:`fno.graph.statuses.node_is_done`
AND ``merge_status == "merged"`` AND an empty ``additional_prs`` - a node with
an open additional PR is live work, whatever its main PR did.
"""

from __future__ import annotations

from typing import Iterable, NamedTuple, Optional

from fno.graph.statuses import node_is_done


class Retirement(NamedTuple):
    """The verdict for one worker row, with the basis it was resolved on."""

    node: Optional[str]  # the resolved node id, None when unresolvable
    node_basis: Optional[str]  # "registry" | "name" | None
    retire: bool
    reason: str  # why, in both directions


def resolve_node(
    name: str, node_field: Optional[str], ids: set
) -> tuple[Optional[str], Optional[str]]:
    """Resolve a worker's node: the registry ``node`` field first, then the name.

    The registry field is authoritative but null on most live rows (the mint
    path fills it in over time), so the fallback reads the worker NAME, whose
    canonical shape is ``<prefix>-<node_id>-<slug>``. Only tokens 1 and 2 are
    consulted, as ``tokens[1:3]`` joined against the full id set, then bare
    ``tokens[1]`` against a hex index of the ids - so a hex-looking slug word
    such as ``feed`` in ``t-d15a-feed-timeout`` is never read as an id. A bare
    hex that matches two graph ids is ambiguous and resolves to nothing.
    """
    if node_field:
        return node_field, "registry"
    tokens = (name or "").split("-")
    if len(tokens) < 2:
        return None, None
    joined = "-".join(tokens[1:3])
    if joined in ids:
        return joined, "name"
    hex_index: dict[str, str] = {}
    ambiguous: set[str] = set()
    for id_ in ids:
        hex_part = id_.rsplit("-", 1)[-1]
        if hex_part in hex_index and hex_index[hex_part] != id_:
            ambiguous.add(hex_part)
        hex_index[hex_part] = id_
    bare = tokens[1]
    if bare in ambiguous:
        return None, None
    if bare in hex_index:
        return hex_index[bare], "name"
    return None, None


def verdicts(rows: Iterable[tuple[str, Optional[str]]], entries=None) -> dict:
    """``(name, node_field)`` roster -> ``{name: Retirement}``, one graph read.

    ``entries`` is the injectable graph (the offline seam, as in
    ``sweep_rows``); when None the graph is loaded once for the whole roster.
    The rule, in order: unresolved node, unknown node, not done, not merged,
    an open additional PR - and only then retire. Rule 5 holds on ANY
    non-empty ``additional_prs`` without asking GitHub: the graph never
    records a merge state for those PRs, a debug view must not make a network
    call per row, and the cost of holding a merged extra PR is one line a
    king checks by hand against the cost of one wrong kill.
    """
    roster = list(rows)
    if entries is None:
        try:
            from fno.graph.load import GRAPH_JSON, load_graph

            if not GRAPH_JSON.exists():
                raise FileNotFoundError(f"no graph at {GRAPH_JSON}")
            entries = load_graph()
        except Exception as exc:  # noqa: BLE001 - fail closed, never act
            return {
                name: Retirement(None, None, False, f"graph-unreadable: {exc}")
                for name, _ in roster
            }
    by_id = {
        e["id"]: e for e in entries if isinstance(e, dict) and e.get("id")
    }
    ids = set(by_id)
    return {
        name: _verdict(name, node_field, ids, by_id)
        for name, node_field in roster
    }


def _verdict(name, node_field, ids, by_id) -> Retirement:
    node, basis = resolve_node(name, node_field, ids)
    if node is None:
        return Retirement(None, None, False, "no-node")
    entry = by_id.get(node)
    if entry is None:
        return Retirement(node, basis, False, "no-such-node")
    if not node_is_done(entry):
        return Retirement(node, basis, False, f"status={entry.get('status')}")
    merge = entry.get("merge_status")
    if merge != "merged":
        return Retirement(node, basis, False, f"merge={merge}")
    extra = entry.get("additional_prs") or []
    if extra:
        nums = ",".join(
            str(p.get("number") if isinstance(p, dict) else p) for p in extra
        )
        return Retirement(node, basis, False, f"extra-pr:{nums}")
    pr = entry.get("pr_number")
    reason = f"done+merged PR {pr}" if pr else "done+merged"
    return Retirement(node, basis, True, reason)
