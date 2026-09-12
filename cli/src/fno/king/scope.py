"""Crown-scope compilation helpers, split out of the retired board module.

`fno.king.board` was retired into the Rust collector (x-25b8, d-450caaeb).
`compile_scope_ids` and `scope_undelivered` outlived it: `fno.pr_watch` reads
scope-compiled rows on every wake tick, and the walk's termination reads the
drain count, which is why they live here instead of dying with the board.
"""

from __future__ import annotations

from typing import Callable, Optional


def compile_scope_ids(scope: str, entries: list[dict], *, resolve=None) -> set[str]:
    """Compile a canonical crown scope into the graph node ids it contains."""
    from fno.agents.crown import _canonical_project, resolve_crown, split_scope

    resolver = resolve or resolve_crown
    level, canonical = resolver(split_scope(scope))
    if level == 2:
        from fno.graph._intake import descendants_of

        by_id = {row.get("id"): row for row in entries if isinstance(row, dict)}
        # A rung-2 scope is a SET of epics; the king's board sees the nodes
        # under EVERY member, not just the first.
        ids: set[str] = set()
        for root_id in split_scope(canonical):
            root = by_id.get(root_id)
            if not root or root.get("type") != "epic":
                raise ValueError(
                    f"crown scope {root_id!r} is not an epic in the graph"
                )
            ids.add(root_id)
            ids.update(descendants_of(entries, root_id))
        return ids

    projects = set(split_scope(canonical))
    return {
        str(row["id"])
        for row in entries
        if isinstance(row, dict)
        and row.get("id")
        and (_canonical_project(str(row.get("project") or "")) or row.get("project"))
        in projects
    }


def scope_undelivered(scope: str, entries: list, resolver: Optional[Callable] = None) -> int:
    """Crown-scope nodes not closed for good: the reign goal's own count.

    The goal keys completion on every node reading done or superseded, so
    termination decisions read this number and no queue. A row with a driver
    leaves the actionable board while its work is unshipped, which is why an
    empty board is a quiet beat and never this count. Closure is
    `is_terminal_entry`, the shared predicate, never a bare completed_at test.
    Deferred stays undelivered: the goal names only done and superseded.
    Raises whatever `compile_scope_ids` raises on an uncompilable scope, so a
    reader that would END a reign on zero must catch and refuse, never read
    the failure as drained.
    """
    from fno.graph.statuses import is_terminal_entry

    kwargs = {"resolve": resolver} if resolver is not None else {}
    ids = compile_scope_ids(scope, entries, **kwargs)
    return sum(
        1
        for row in entries
        if isinstance(row, dict)
        and str(row.get("id") or "") in ids
        and not is_terminal_entry(row)
    )
