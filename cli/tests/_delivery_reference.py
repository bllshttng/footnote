"""Test-side reference of the Rust delivery classifier (scoreboard.rs).

The decision lives in the keeper; this stub exists so fold tests stay
hermetic on boxes without the worker binary. It mirrors the decision table:
a merge delivers with no ledger row, an explicit doc/delivery terminal
delivers a known node, a ship-only terminal never delivers a known unmerged
node, and a terminal on a node the graph lost counts as inferred. A fold-test
failure that points here means the stub and the keeper disagree.
"""

from __future__ import annotations


def reference_deliveries(graph_nodes: list[dict], rows: list[dict]) -> dict:
    from fno.terminals import DELIVERED_TERMINALS

    doc = {"DoneAdvisory"}
    delivery = {"DoneDelivery"}

    by_id: dict[str, dict] = {}
    for n in graph_nodes or []:
        if isinstance(n, dict) and n.get("id"):
            by_id[n["id"]] = n

    by_node: dict[str, dict] = {}
    for nid, n in by_id.items():
        merged = n.get("merge_status") == "merged"
        by_node[nid] = {
            "class": "merged" if merged else "no_evidence",
            "delivered": merged,
            "confirmed": merged,
            "evidence": "graph_merge" if merged else "none",
            "ship_ts": n.get("merged_at") or n.get("completed_at"),
            "node_known": True,
        }

    # First delivered-kind row wins as the evidence, mirroring the keeper.
    best_kind: dict[str, str] = {}
    best_ts: dict[str, str] = {}
    for r in rows or []:
        if not isinstance(r, dict):
            continue
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
        n = by_id.get(nid)
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
        if kind == "ship" or n.get("merge_status") == "merged":
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
    return by_node
