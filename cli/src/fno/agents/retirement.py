"""The single owner of "has this worker's node already shipped" (x-1379).

The join between a ``fno agents top`` worker row and the graph. Fail-closed
on every read it cannot trust: an absence of reported doneness is not
doneness, because a human decides whether to kill a session from this
verdict. Doneness is :func:`fno.graph.statuses.node_is_done` AND
``merge_status == "merged"`` AND an empty ``additional_prs``.
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
    """Registry ``node`` field first, then the worker name.

    The field is authoritative but null on most live rows, so the fallback
    reads ``<prefix>-<node_id>-<slug>``: tokens 1 and 2 only, ``tokens[1:3]``
    joined against the full ids, then bare ``tokens[1]`` against a hex index
    - so a slug word like ``feed`` in ``t-d15a-feed-timeout`` is never read
    as an id, and a bare hex matching two graph ids resolves to nothing.
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
    ``sweep_rows``). The rule, in order: unresolved node, unknown node, not
    done, not merged, an open additional PR - only then retire. Rule 5 holds
    on ANY non-empty ``additional_prs`` without asking GitHub: the graph
    records no merge state for those PRs and a debug view must not make a
    network call per row; holding a merged extra PR costs one line.
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
