"""Reconcile's self-heal sweeps: what a merged PR closes, and who shipped it.

Each takes a plain ``entries`` list and mutates it in place. Every one runs
inside the store's mutator, under the lock, so none may call a writer that
goes through the keeper.

They live here rather than in ``graph/cli.py`` because that file is over the
source budget and may only shrink. ``graph/cli.py`` re-exports both names.
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
    """Give every node that shipped inside another node's PR the `do` rows of
    the sessions that shipped it.

    Nothing writes `sessions[]` on the close path. Every writer is keyed to a
    session that OWNS the node - it was spawned with `--node`, it holds the
    claim, or its manifest names the node - and reconcile owns no node. So one
    worker that claims one node and ships several leaves its passengers with a
    merged PR, a real code change inside it, and no session at all.

    The evidence of carriage is the shared PR: a node linked to a PR, with no
    session of its own, and a peer linked to that same PR that has one. Only
    the `do` rows travel. `blueprint` and `ship` happened to the owner's node,
    while the do phase is the one whose work reached the passenger's files.

    The key is the pr_url, never the bare number. The graph spans five repos
    whose PR numbers already interleave, so a number alone would stamp one
    repo's session onto another repo's node. A link with no url is skipped
    rather than matched on the number: refusing to guess is the whole point.
    Links come from :func:`node_pr_refs`, so a PR carried in `additional_prs`
    counts on both sides.

    Mutates in place and never calls :func:`append_session_record`: this runs
    inside reconcile's mutator, already under the store lock, and that writer
    goes through the keeper.
    """
    import copy as _copy

    from fno.graph._reconcile import node_pr_refs

    def _urls(node: dict) -> list[str]:
        # `.get` throughout, and a guarded call: this runs inside the mutator,
        # where a raise on one malformed row aborts the close it rides on.
        try:
            return [u for _n, u in node_pr_refs(node) if isinstance(u, str) and u]
        except Exception:  # noqa: BLE001 - an unreadable row contributes nothing
            return []

    donors: dict[str, list[dict]] = {}
    for e in entries:
        if not isinstance(e, dict):
            continue
        rows = e.get("sessions")
        if not isinstance(rows, list):
            continue
        do_rows = [r for r in rows if isinstance(r, dict) and r.get("phase") == "do"]
        if not do_rows:
            continue
        for url in _urls(e):
            donors.setdefault(url, []).extend(do_rows)

    stamped: list[str] = []
    for e in entries:
        if not isinstance(e, dict) or e.get("sessions"):
            continue
        nid = e.get("id")
        if not isinstance(nid, str) or not nid:
            continue
        carried: list[dict] = []
        seen: set[tuple] = set()
        for url in _urls(e):
            for row in donors.get(url, ()):
                key = (row.get("phase"), row.get("harness"), row.get("session_id"))
                if key in seen:
                    continue
                seen.add(key)
                # deepcopy: two nodes must not share one row, nor the nested
                # observed_model / merge_grant dicts inside it.
                carried.append(_copy.deepcopy(row))
        if carried:
            e["sessions"] = carried
            stamped.append(nid)
    return stamped
