"""One plan-level hold reader shared by every PR merge path."""
from __future__ import annotations

from typing import Optional

from fno.graph.ladder import DispatchHoldState, DispatchHoldVerdict, dispatch_hold_verdict


class HoldLookupError(RuntimeError):
    """The merge path could not prove whether its bound plan is held."""


def _pr_url(pr_number: int, cwd: str) -> str:
    from fno.pr import _merge
    from fno.pr._proc import ToolMissing

    try:
        result = _merge._gh(
            ["pr", "view", str(pr_number), "--json", "url", "-q", ".url"],
            cwd,
        )
    except ToolMissing as exc:
        raise HoldLookupError("gh CLI is unavailable") from exc
    if not result.ok or not result.stdout.strip():
        raise HoldLookupError("PR URL is unreadable; cannot scope the hold lookup")
    return result.stdout.strip()


def hold_for_pr(pr_number: int, cwd: str) -> Optional[DispatchHoldVerdict]:
    """Return the held/invalid plan ancestry for a PR, or None when unheld."""
    from fno.graph.store import read_graph
    from fno.paths import graph_json
    from fno.pr._merge import _find_pr_node_id
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        # A hold is a footnote-graph-resident concept: under an external
        # tracker backend this repo's graph.json is not the delivery record
        # of truth, so there is no plan hold to read - never "unreadable".
        return None

    try:
        entries = read_graph(graph_json())
    except Exception as exc:  # noqa: BLE001 - hold reads fail closed
        raise HoldLookupError(f"backlog graph is unreadable: {exc}") from exc
    from fno.graph._reconcile import node_pr_refs

    if not any(
        number == pr_number
        for entry in entries
        if isinstance(entry, dict)
        for number, _url in node_pr_refs(entry)
    ):
        return None
    url = _pr_url(pr_number, cwd)
    node_id = _find_pr_node_id(entries, pr_number, url)
    if node_id is None:
        # No graph-bound delivery means there is no Footnote plan hold to read.
        # A same-number node in another repo is deliberately not a match.
        return None
    by_id = {
        entry.get("id"): entry
        for entry in entries
        if isinstance(entry, dict) and entry.get("id")
    }
    node = by_id.get(node_id)
    if not isinstance(node, dict):
        raise HoldLookupError(f"graph node {node_id} disappeared during hold lookup")
    return dispatch_hold_verdict(node, by_id)


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
