"""One plan-level hold reader shared by every PR merge path."""
from __future__ import annotations

from typing import Optional

from fno.graph.ladder import DispatchHoldState, DispatchHoldVerdict, dispatch_hold_verdict


class HoldLookupError(RuntimeError):
    """The merge path could not prove whether its bound plan is held."""


def hold_for_pr(pr_number: int, cwd: str) -> Optional[DispatchHoldVerdict]:
    """Return the held/invalid plan ancestry for a PR, or None when unheld.

    Checks BOTH the ref-stamped node (``_find_pr_node_id``) and every node
    named on the PR's exact ``Backlog-Closure`` trailer - a trailer-only
    claim (a node never individually stamped at creation) was invisible to
    the ref-based match alone, so a held node named only on the trailer
    passed this gate and closed post-merge via ``bind_closure_claims``,
    which performs no hold check of its own (round-10 review fix).
    """
    from fno.graph.store import read_graph
    from fno.paths import graph_json
    from fno.pr._merge import _find_pr_node_id
    from fno.pr.closure import ClosureQueryError, fetch_pr_closure_context, parse_closure_trailer

    try:
        entries = read_graph(graph_json())
    except Exception as exc:  # noqa: BLE001 - hold reads fail closed
        raise HoldLookupError(f"backlog graph is unreadable: {exc}") from exc
    from fno.graph._reconcile import node_pr_refs

    stamped = any(
        number == pr_number
        for entry in entries
        if isinstance(entry, dict)
        for number, _url in node_pr_refs(entry)
    )

    try:
        pr_ctx = fetch_pr_closure_context(pr_number, cwd=cwd)
    except ClosureQueryError as exc:
        if not stamped:
            # Nothing ref-stamped and the trailer is unreadable: there is no
            # claim of either kind to hold-check.
            return None
        raise HoldLookupError(f"PR body is unreadable; cannot scope the hold lookup: {exc}") from exc

    ref_node_id: Optional[str] = None
    if stamped:
        ref_node_id = _find_pr_node_id(entries, pr_number, pr_ctx.url or "")

    candidate_ids: list[str] = []
    if ref_node_id is not None:
        candidate_ids.append(ref_node_id)
    for claimed in parse_closure_trailer(pr_ctx.body):
        if claimed not in candidate_ids:
            candidate_ids.append(claimed)

    if not candidate_ids:
        # No graph-bound delivery of either kind means there is no Footnote
        # plan hold to read. A same-number node in another repo is
        # deliberately not a match.
        return None

    by_id = {
        entry.get("id"): entry
        for entry in entries
        if isinstance(entry, dict) and entry.get("id")
    }
    for node_id in candidate_ids:
        node = by_id.get(node_id)
        if not isinstance(node, dict):
            if node_id == ref_node_id:
                raise HoldLookupError(f"graph node {node_id} disappeared during hold lookup")
            # A trailer can name an id this graph slice does not carry (a
            # typo, or another project's node) - nothing to hold-check.
            continue
        verdict = dispatch_hold_verdict(node, by_id)
        if verdict is not None:
            return verdict
    return None


def merge_hold_reason(pr_number: int, cwd: str) -> Optional[str]:
    """A human-readable refusal shared by sanctioned and direct merge paths."""
    try:
        verdict = hold_for_pr(pr_number, cwd)
    except HoldLookupError as exc:
        return f"dispatch-hold-invalid: {exc}; refusing to assume unheld"
    if verdict is None:
        return None
    hold = verdict.hold
    if hold.state is DispatchHoldState.INVALID:
        return f"{verdict.guard_reason}: {hold.detail}; refusing to assume unheld"
    return (
        f"{verdict.guard_reason}: {hold.reason}; set_by={hold.set_by}; "
        f"release_when={hold.release_when}; review_on={hold.review_on}"
    )
