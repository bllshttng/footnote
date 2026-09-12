"""The evidence one `backlog next` selection reads.

Who holds a live `node:` claim, and which nodes a live worker is on: two
questions the selector, the observer, and the starvation receipts each used to
ask for themselves, paying a subprocess-bound read every time for the same
answer. Asked once here, beside the receipts that consume them. Homed out of
graph/cli.py: that file is over the source budget.
"""
from __future__ import annotations

from typing import Optional


class OccupancyUnavailable(RuntimeError):
    """A strict occupancy source could not be read; selection must refuse."""


def read_occupancy(entries: list[dict], claimed_reader) -> tuple[set, dict]:
    """Live `node:` claims and roster-worked node ids, read once, together.

    Both reads are subprocess-bound and independent, so they overlap instead of
    queueing. Both stay strict: an unreadable source raises, and the caller
    refuses rather than select against an empty occupancy set. The claim
    verdict is read first, so its refusal wins when both sources are down.
    """
    from concurrent.futures import ThreadPoolExecutor

    from fno.graph.statuses import live_worked_node_ids

    with ThreadPoolExecutor(max_workers=2) as pool:
        claim_read = pool.submit(claimed_reader, strict=True)
        worked_read = pool.submit(live_worked_node_ids, strict=True, entries=entries)
        try:
            claimed = claim_read.result()
        except Exception as exc:  # noqa: BLE001 - unknown claim state refuses
            raise OccupancyUnavailable("live claim state is unavailable") from exc
        try:
            worked = worked_read.result()
        except Exception as exc:  # noqa: BLE001 - unknown liveness refuses
            raise OccupancyUnavailable(f"worked overlay unreadable: {exc}") from exc
    return set(claimed), worked


def _starvation_receipts(
    entries: list[dict],
    project_filter: Optional[str],
    all_: bool,
    scope_ids: Optional[set],
    claimed: set,
    now,
    staleness_days: int,
    *,
    mission: Optional[str] = None,
    roadmap_id: Optional[str] = None,
) -> list[tuple[str, str]]:
    """Classify why each ready-ish in-scope node was NOT selected (G1 receipts).

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.backlog.advance import first_dead_ancestor, selection_guards
    from fno.graph._intake import filter_by_project
    # Lazy: cli imports this module, so a module-level import would cycle.
    from fno.graph.cli import (
        _container_ids,
        _has_unmerged_open_pr,
        _is_batched_member,
    )
    from fno.graph.strand import _is_live

    container_ids = _container_ids(entries)
    # One pass, guarding against a non-dict row (codebase convention: a malformed
    # entry must not AttributeError the cold receipt path).
    by_id: dict = {}
    ready_ish_rows: list[dict] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if e.get("id"):
            by_id[e["id"]] = e
        # `design` rides along with ready/idea: it is buildable-looking work a
        # human can still name explicitly, so a null `next` must explain it
        # rather than drop it silently - the exact starvation this receipt
        # exists to prevent (a backlog that is ALL design-stage would otherwise
        # return null with nothing to say).
        if e.get("status") not in ("ready", "design", "idea") or e.get("completed_at"):
            continue
        if roadmap_id and e.get("roadmap_id") != roadmap_id:
            continue
        if mission and e.get("mission_id") != mission:
            continue
        ready_ish_rows.append(e)
    ready_ish = filter_by_project(ready_ish_rows, project_filter, all_)
    if scope_ids is not None:
        ready_ish = [e for e in ready_ish if e.get("id") in scope_ids]
    out: list[tuple[str, str]] = []
    for e in ready_ish:
        nid = e.get("id")
        if not nid:
            continue
        # A plan hold outranks structural exclusions: the owner may also be a
        # container and a descendant may carry no plan of its own, but the
        # actionable reason every dispatcher must report is the attributable
        # hold on their shared delivery ancestry.
        hold_guard = selection_guards(e, by_id, now, staleness_days=staleness_days)
        if hold_guard and hold_guard.startswith("dispatch-hold"):
            reason = hold_guard
        elif first_dead_ancestor(
            e, by_id, is_dead=lambda anc: not _is_live(anc)
        ) and not (
            e.get("contained_in") or _has_unmerged_open_pr(e) or _is_batched_member(e)
        ):
            # Terminal-ancestor arm (x-a31a): the structural cause outranks
            # incidental attributes - a plan-less node under a dead parent
            # reads here, not plan-less. Superseded/deferred are a subset of
            # terminal, so this arm owns the old selection-guards
            # dead-ancestor classification; contained, in-review, and batched
            # nodes fall through so their classifications stand.
            reason = "dead-ancestor"
        elif not e.get("plan_path"):
            reason = "plan-less"
        elif nid in container_ids:
            reason = "container"
        elif nid in claimed:
            reason = "claimed"
        elif e.get("status") in ("design", "idea"):
    # Rationale (9 lines): docs/architecture/graph-cli-rationale.md#starvation-receipts-4190
            reason = e["status"]
        elif e.get("status") == "ready" and (_has_unmerged_open_pr(e) or _is_batched_member(e)):
            continue  # in review / batched - handled, not starved
        else:
            g = hold_guard
            if not g:
                continue  # no known exclusion (would have been selected)
            if g.startswith("contained"):
                # Not starvation either: the work IS being delivered, inside
                # another node's PR. Left in the generic `quarantined` bucket it
                # read as stale work needing attention, and a decomposed epic
                # printed one bogus line per adopted node on every `next` until
                # its unit merged - permanent noise the operator cannot act on.
                reason = "contained"
            elif g == "design-stage":
                # Not starvation: planned but not blueprinted, so it reads as
                # its own rung rather than the generic quarantine bucket.
                reason = "design"
            elif g == "idea-stage":
                # Also not starvation: a linked-but-undesigned doc (a decompose
                # scaffold, or a plan hand-edited back down). Named separately
                # from `design` so the receipt says which pass it is waiting on.
                reason = "idea"
            else:
                reason = "quarantined"
        out.append((nid, reason))
    return out
