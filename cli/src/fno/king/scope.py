"""Crown-scope compilation helpers, split out of the retired board module.

`fno.king.board` was retired into the Rust collector (x-25b8, d-450caaeb).
`compile_scope_ids` and `KING_PRIORITIES` outlived it: `fno.pr_watch` reads
scope-compiled rows on every wake tick, which is why they live here instead of
dying with the board.
"""

from __future__ import annotations

#: Priorities a king treats as its own work. Lower bands are the operator's to
#: rank up; a king that dispatched p2 would spend the fleet on the wrong thing.
KING_PRIORITIES = frozenset({"p0", "p1"})


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
