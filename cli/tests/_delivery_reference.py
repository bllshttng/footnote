"""Test-side reference of the Rust delivery classifier (scoreboard.rs).

The decision lives in the keeper; this stub exists so fold tests stay
hermetic on boxes without the worker binary. It mirrors the decision table:
a merge (status or merged_at) delivers with no ledger row, an explicit
doc/delivery terminal delivers a known node, a ship-only terminal never
delivers a known unmerged node, and a terminal on a node the graph lost
counts as inferred. A fold-test failure that points here means the stub and
the keeper disagree.
"""

from __future__ import annotations


def reference_deliveries(graph_nodes: list[dict], rows: list[dict], project: str | None = None) -> dict:
    from fno.terminals import DELIVERED_TERMINALS

    doc = {"DoneAdvisory"}
    delivery = {"DoneDelivery"}

    all_nodes = [n for n in graph_nodes or [] if isinstance(n, dict) and n.get("id")]
    all_rows = [r for r in rows or [] if isinstance(r, dict)]
    if project:
        node_ids = {n["id"] for n in all_nodes if n.get("project") == project}
        nodes = [n for n in all_nodes if n.get("project") == project]
        rows_p = [r for r in all_rows if r.get("project") == project]
        unattributed = sum(1 for r in all_rows if not r.get("project"))
        scope = {
            "project": project,
            "nodes": len(node_ids),
            "unattributed_rows": unattributed,
            "other_project_rows": len(all_rows) - len(rows_p) - unattributed,
        }
    else:
        node_ids = {n["id"] for n in all_nodes}
        nodes = all_nodes
        rows_p = all_rows
        scope = None

    by_node: dict[str, dict] = {}
    for n in nodes:
        merged = n.get("merge_status") == "merged" or bool(n.get("merged_at"))
        by_node[n["id"]] = {
            "class": "merged" if merged else "no_evidence",
            "delivered": merged,
            "confirmed": merged,
            "evidence": "graph_merge" if merged else "none",
            "ship_ts": n.get("merged_at") or n.get("completed_at"),
            "node_known": True,
        }

    best_kind: dict[str, str] = {}
    best_ts: dict[str, str] = {}
    for r in rows_p:
        nid = r.get("graph_node_id")
        if not isinstance(nid, str) or not nid:
            continue  # junk never crashes the fold, mirroring the keeper
        tr = r.get("termination_reason")
        if not isinstance(tr, str) or tr not in DELIVERED_TERMINALS:
            continue
        if nid not in best_kind:
            best_kind[nid] = "doc" if tr in doc else "delivery" if tr in delivery else "ship"
        best_ts[nid] = r.get("completed")

    for nid, kind in best_kind.items():
        n = next((x for x in nodes if x["id"] == nid), None)
        if n is None:
            by_node[nid] = {
                "class": "inferred",
                "delivered": True,
                "confirmed": False,
                "evidence": "session_terminal",
                "ship_ts": best_ts[nid],
                "node_known": False,
            }
            continue
        if kind == "ship" or n.get("merge_status") == "merged" or n.get("merged_at"):
            continue  # the node pass already classified it honestly
        cls = "delivered_doc" if kind == "doc" else "delivered_delivery"
        by_node[nid] = {
            "class": cls,
            "delivered": True,
            "confirmed": True,
            "evidence": "doc_terminal" if kind == "doc" else "delivery_terminal",
            "ship_ts": n.get("merged_at") or n.get("completed_at") or best_ts[nid],
            "node_known": True,
        }

    result: dict = {
        "by_node": by_node,
        "coverage": {
            "nodes": len(nodes),
            "rows_with_node": sum(1 for r in rows_p if isinstance(r.get("graph_node_id"), str)),
            "rows_without_node": sum(1 for r in rows_p if not r.get("graph_node_id")),
            "inferred_nodes": sum(1 for c in by_node.values() if c["class"] == "inferred"),
        },
    }
    if scope is not None:
        result["coverage"]["project_scope"] = scope
        result["scoped"] = {
            "entries": nodes,
            "rows": rows_p,
            "node_ids": sorted(node_ids),
            "scope": scope,
        }
    return result
