"""Why this node and not that one: the selection cascade, made answerable.

`fno backlog next` narrows the open graph through a fixed sequence of filters
and then sorts what survives. Every drop was silent. An operator asking why a
ready node never launched had no instrument, and the 2026-09-01 orchestration
audit had to reconstruct the answer by reading source.

The cascade lives HERE and `cmd_next._pick_ready` consumes it, rather than the
explanation reimplementing the same ten filters beside the selector. A parallel
implementation is a second selector that lies the moment either one moves, and
an explanation that disagrees with the selection is worse than none.

A filter narrows a LIST, not a row. That is not indirection for its own sake:
`filter_by_project` resolves the project by DETECTING it from the candidates it
is handed, so it cannot be expressed as a per-row predicate without changing
what it does.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Optional


@dataclass(frozen=True)
class SelectionFilter:
    """One narrowing step, with the sentence an operator needs when it bites."""

    #: Stable identifier, printed by `advance --explain` and safe to grep for.
    name: str
    #: Why a node dropped here, in terms of what the operator can do about it.
    why: str
    narrow: Callable[[list[dict]], list[dict]]


@dataclass
class CascadeResult:
    """What survived, what each filter took, and where each node fell out."""

    survivors: list[dict] = field(default_factory=list)
    #: filter name -> how many candidates it removed, in cascade order.
    drops: list[tuple[str, int]] = field(default_factory=list)
    #: node id -> the name of the first filter that removed it. A node absent
    #: from this map and from `survivors` was never a candidate at all.
    dropped_by: dict[str, str] = field(default_factory=dict)

    def reason_for(self, node_id: str) -> Optional[str]:
        """The filter that dropped ``node_id``, or None if it survived."""
        return self.dropped_by.get(node_id)


def run_cascade(candidates: list[dict], filters: list[SelectionFilter]) -> CascadeResult:
    """Apply ``filters`` in order, recording what each one took.

    Attribution is to the FIRST filter that removes a node. A node dropped by
    the project filter may also be a container and also be batched; naming all
    three would bury the one an operator has to act on.
    """
    result = CascadeResult()
    current = list(candidates)
    for f in filters:
        before = {e.get("id") for e in current if e.get("id")}
        current = f.narrow(current)
        after = {e.get("id") for e in current if e.get("id")}
        gone = before - after
        result.drops.append((f.name, len(gone)))
        for node_id in gone:
            result.dropped_by.setdefault(node_id, f.name)
    result.survivors = current
    return result


def build_selection_filters(
    entries: list[dict],
    *,
    roadmap_id: Optional[str],
    mission: Optional[str],
    parent_target_id: Optional[str],
    project_filter: Optional[str],
    all_: bool,
    claimed: "set[str] | frozenset[str]",
    container_ids: "set[str] | frozenset[str]",
) -> list[SelectionFilter]:
    """The cascade `fno backlog next` runs, in its exact shipped order.

    ``claimed`` and ``container_ids`` are passed in rather than computed here
    because the caller already holds them under its graph lock; recomputing
    would read a different instant than the selection it is explaining.
    """
    from datetime import datetime, timezone

    from fno.graph._intake import descendants_of, filter_by_project

    fs: list[SelectionFilter] = []

    if roadmap_id:
        fs.append(
            SelectionFilter(
                "roadmap",
                f"not on roadmap {roadmap_id}",
                lambda c: [e for e in c if e.get("roadmap_id") == roadmap_id],
            )
        )
    if mission:
        fs.append(
            SelectionFilter(
                "mission",
                f"not in mission {mission}",
                lambda c: [e for e in c if e.get("mission_id") == mission],
            )
        )
    if parent_target_id is not None:
        scope = descendants_of(entries, parent_target_id)
        fs.append(
            SelectionFilter(
                "parent-scope",
                f"not a descendant of {parent_target_id}",
                lambda c: [e for e in c if e.get("id") in scope],
            )
        )

    fs.append(
        SelectionFilter(
            "project",
            "belongs to another project (pass --all to widen, or --project)",
            lambda c: filter_by_project(c, project_filter, all_),
        )
    )

    if claimed:
        fs.append(
            SelectionFilter(
                "live-claim",
                "a live session already holds node:<id>; check `fno agents claim status`",
                lambda c: [e for e in c if e.get("id") not in claimed],
            )
        )

    def _drop_open_pr(c: list[dict]) -> list[dict]:
        from fno.graph.cli import _has_unmerged_open_pr

        return [e for e in c if e.get("status") != "ready" or not _has_unmerged_open_pr(e)]

    fs.append(
        SelectionFilter(
            "unmerged-open-pr",
            "already carries a PR that has not merged; the work is in review, not waiting",
            _drop_open_pr,
        )
    )

    fs.append(
        SelectionFilter(
            "container",
            "an epic is never built directly; its work lives in its children",
            lambda c: [e for e in c if e.get("id") not in container_ids],
        )
    )

    def _drop_batched(c: list[dict]) -> list[dict]:
        from fno.graph.cli import _is_batched_member

        return [e for e in c if not _is_batched_member(e)]

    fs.append(
        SelectionFilter(
            "batched",
            "committed to an open batch; it ships via the batch PR",
            _drop_batched,
        )
    )

    def _drop_guarded(c: list[dict]) -> list[dict]:
        from fno.backlog.advance import _guard_staleness_days, selection_guards

        guard_now = datetime.now(timezone.utc)
        guard_stale = _guard_staleness_days()
        guard_by_id = {e.get("id"): e for e in entries if e.get("id")}
        return [
            e
            for e in c
            if not selection_guards(e, guard_by_id, guard_now, staleness_days=guard_stale)
        ]

    fs.append(
        SelectionFilter(
            "selection-guard",
            "under a dead ancestor, or ready and untouched past the staleness window",
            _drop_guarded,
        )
    )
    return fs
