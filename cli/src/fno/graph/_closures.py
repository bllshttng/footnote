"""Reconcile's self-heal sweeps: what a merged PR closes, and who shipped it.

Each mutates a plain ``entries`` list in place, inside the store's mutator and
under the lock, so none may call a writer that goes through the keeper. They
live here because ``graph/cli.py`` is over budget; it re-exports every name.
"""
from __future__ import annotations


def _strandable_epic_ids(entries: list[dict]) -> set[str]:
    """Open epics (parents) whose children are ALL done - closeable right now.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.graph._reconcile import _reopen_outranks_child_closes

    children_by_parent: dict[str, list[dict]] = {}
    for e in entries:
        if isinstance(e, dict) and isinstance(e.get("parent"), str):
            children_by_parent.setdefault(e["parent"], []).append(e)
    id_to_entry = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    out: set[str] = set()
    for pid, kids in children_by_parent.items():
        parent = id_to_entry.get(pid)
        if (
            parent is not None
            and not parent.get("completed_at")
            and all(k.get("completed_at") for k in kids)
            and not _reopen_outranks_child_closes(parent, kids)
        ):
            out.add(pid)
    return out


def _sweep_close_done_epics(entries: list[dict]) -> list[str]:
    """Close every open epic whose children are all done (self-heal/migration).
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    # Local: graph/cli.py imports this module, so a module-level import back
    # into it would be a cycle.
    from fno.graph.cli import _apply_completion_fields, _auto_closed_note

    id_to_entry = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    closed: list[str] = []
    for _ in range(64):  # fixpoint, depth-capped against a malformed cycle
        ready = _strandable_epic_ids(entries)
        if not ready:
            break
        for pid in ready:
            parent = id_to_entry.get(pid)
            if parent is None or parent.get("completed_at"):
                continue
            _apply_completion_fields(parent)
            if not parent.get("completion_note"):
                parent["completion_note"] = _auto_closed_note(parent)
            closed.append(pid)
    return closed


def _sweep_stamp_carried_sessions(entries: list[dict]) -> list[str]:
    """Give a node carried inside another node's PR the `do` rows that shipped it.

    Every writer of ``sessions[]`` is keyed to a session that OWNS the node, and
    reconcile owns none, so a worker that claims one node and ships several
    leaves its passengers with a merged PR and no session at all. The evidence
    of carriage is the shared PR. Only `do` travels: blueprint and ship happened
    to the owner's node. The key is the pr_url, because five repos share this
    graph and their PR numbers interleave; a link with no url is skipped rather
    than matched on the number. Links come from :func:`node_pr_refs`, so
    `additional_prs` counts on both sides.
    """
    import copy as _copy

    from fno.graph._reconcile import node_pr_refs

    def _urls(node: dict) -> list[str]:
        try:  # guarded: a raise here would abort the close this rides on
            return [u for _n, u in node_pr_refs(node) if isinstance(u, str) and u]
        except Exception:  # noqa: BLE001 - an unreadable row contributes nothing
            return []

    donors: dict[str, list[dict]] = {}
    for e in entries:
        rows = e.get("sessions") if isinstance(e, dict) else None
        if not isinstance(rows, list):
            continue
        do_rows = [r for r in rows if isinstance(r, dict) and r.get("phase") == "do"]
        for url in _urls(e) if do_rows else ():
            donors.setdefault(url, []).extend(do_rows)

    stamped: list[str] = []
    for e in entries:
        if not isinstance(e, dict) or e.get("sessions"):
            continue
        nid = e.get("id")
        if not isinstance(nid, str) or not nid:
            continue
        # deepcopy: two nodes must not share a row, nor the nested
        # observed_model / merge_grant dicts inside it.
        carried: dict[tuple, dict] = {}
        for url in _urls(e):
            for row in donors.get(url, ()):
                key = (row.get("phase"), row.get("harness"), row.get("session_id"))
                carried.setdefault(key, _copy.deepcopy(row))
        if carried:
            e["sessions"] = list(carried.values())
            stamped.append(nid)
    return stamped
