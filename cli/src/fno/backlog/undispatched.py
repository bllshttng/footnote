"""Independent inventory of planned work with no execution claim."""

from __future__ import annotations

from pathlib import Path
from typing import Any


OBSERVER_COMMAND = "fno backlog undispatched --json"


class ObserverReadError(RuntimeError):
    """The independent observer could not establish a complete input."""


def _validate_rows(value: Any, source: str) -> list[dict]:
    if not isinstance(value, list):
        raise ValueError(f"{source} unreadable: expected a list")
    rows: list[dict] = []
    for row in value:
        if not isinstance(row, dict):
            raise ValueError(f"{source} unreadable: expected object rows")
        rows.append(row)
    return rows


def _descendants(entries: list[dict], parent_id: str) -> set[str]:
    children: dict[str, list[str]] = {}
    for entry in entries:
        node_id = entry.get("id")
        parent = entry.get("parent")
        if isinstance(node_id, str) and isinstance(parent, str):
            children.setdefault(parent, []).append(node_id)
    found: set[str] = set()
    frontier = list(children.get(parent_id, []))
    while frontier:
        node_id = frontier.pop()
        if node_id in found:
            continue
        found.add(node_id)
        frontier.extend(children.get(node_id, []))
    return found


def _has_pr(entry: dict) -> bool:
    if entry.get("pr_number"):
        return True
    return any(
        isinstance(extra, dict) and extra.get("number")
        for extra in (entry.get("additional_prs") or [])
    )


def _blocked(entry: dict, by_id: dict[str, dict]) -> bool:
    for blocker_id in entry.get("blocked_by") or []:
        blocker = by_id.get(blocker_id)
        if blocker is None or (
            blocker.get("status") != "done" and not blocker.get("completed_at")
        ):
            return True
    return False


def classify_planned_unclaimed(
    entries: list[dict],
    claims: list[dict],
    *,
    project: str | None = None,
    mission: str | None = None,
    roadmap_id: str | None = None,
    parent: str | None = None,
) -> dict:
    if not isinstance(entries, list) or isinstance(entries, dict):
        raise ValueError("graph unreadable: expected an entries list")
    entries = _validate_rows(entries, "graph")
    claims = _validate_rows(claims, "claims")
    by_id = {
        entry["id"]: entry
        for entry in entries
        if isinstance(entry.get("id"), str) and entry.get("id")
    }
    claimed: dict[str, str] = {}
    for claim in claims:
        key = claim.get("key")
        if not isinstance(key, str):
            raise ValueError("claims unreadable: claim key is not a string")
        if key.startswith("node:"):
            claimed[key.removeprefix("node:")] = str(claim.get("state") or "unknown")

    child_ids = {
        entry.get("parent")
        for entry in entries
        if isinstance(entry.get("parent"), str)
    }
    rows: list[dict] = []
    for entry in entries:
        node_id = entry.get("id")
        if not isinstance(node_id, str) or not node_id:
            raise ValueError("graph unreadable: entry id is not a string")
        if project is not None and entry.get("project") != project:
            continue
        if mission is not None and entry.get("mission_id") != mission:
            continue
        if roadmap_id is not None and entry.get("roadmap_id") != roadmap_id:
            continue
        if parent is not None and node_id not in _descendants(entries, parent):
            continue
        claim_state = claimed.get(node_id)
        facts = {
            "status_ready": entry.get("status") == "ready",
            "plan_finalized": isinstance(entry.get("plan_path"), str)
            and bool(entry["plan_path"].strip()),
            "leaf": entry.get("type") != "epic" and node_id not in child_ids,
            "completed": bool(entry.get("completed_at")),
            "has_pr": _has_pr(entry),
            "batch_owner": bool(entry.get("batch")),
            "blocked": _blocked(entry, by_id),
            # Containment folds a node into its owner's dispatch ("never
            # dispatch alone"): it ships inside the owner's PR, so a free
            # claim of its own does not make it offerable here.
            "contained": bool(entry.get("contained_in")),
            "claim_state": claim_state,
        }
        if not (
            facts["status_ready"]
            and facts["plan_finalized"]
            and facts["leaf"]
            and not facts["completed"]
            and not facts["has_pr"]
            and not facts["batch_owner"]
            and not facts["blocked"]
            and not facts["contained"]
            and claim_state is None
        ):
            continue
        rows.append(
            {
                "id": node_id,
                "priority": entry.get("priority"),
                "domain": entry.get("domain"),
                "plan_path": entry.get("plan_path"),
                "facts": facts,
                **{
                    key: entry.get(key)
                    for key in ("title", "project", "mission_id", "roadmap_id", "parent")
                    if entry.get(key) is not None
                },
            }
        )

    # One ordering for both queues. Sorting by (priority, id string) here made
    # the stop hook's `next:` line name a node the rank-aware drain never
    # picked next. Reuse the shared key; never re-implement it.
    from fno.graph._intake import make_selection_sort_key

    live = frozenset(n for n, state in claimed.items() if state == "live")
    # The keys-only snapshot carries state null, so `live` is empty on the
    # real path: the observer no longer floats an epic whose only progress
    # signal is a live claim on a sibling. Presence-only by contract; the
    # in-progress signal survives via node status/session_id/completed_at.
    # Test callers may still pass state-carrying rows.
    # The key sorts full entries; a row is a projection that already dropped
    # rank and created_at, so sort through `by_id` rather than through the row.
    order = make_selection_sort_key(entries, live_claimed=live)
    rows.sort(key=lambda row: (order(by_id[row["id"]]), str(row["id"])))
    return {
        "source": OBSERVER_COMMAND,
        "status": "ok",
        "entries_scanned": len(entries),
        "claims_scanned": len(claims),
        "rows": rows,
    }


def prepend_missed_rows(
    normal_rows: list[dict], observer_receipt: dict
) -> tuple[list[dict], list[dict]]:
    """Return observer-only rows before the normal selector frontier."""
    normal_ids = {row.get("id") for row in normal_rows}
    missed = [
        row
        for row in observer_receipt.get("rows", [])
        if row.get("id") not in normal_ids
    ]
    return [*missed, *normal_rows], missed


def read_planned_unclaimed(
    *,
    graph_path: Path | None = None,
    project: str | None = None,
    mission: str | None = None,
    roadmap_id: str | None = None,
    parent: str | None = None,
) -> dict:
    """Read both independent inputs and return a positive observer receipt."""
    from fno.graph.store import read_graph_strict

    try:
        entries = read_graph_strict(graph_path) if graph_path is not None else read_graph_strict()
    except Exception as exc:  # noqa: BLE001 - identify the failed source
        raise ObserverReadError(f"graph unreadable: {exc}") from exc
    return read_planned_unclaimed_from_entries(
        entries,
        project=project,
        mission=mission,
        roadmap_id=roadmap_id,
        parent=parent,
    )


def read_claim_snapshot() -> list[dict]:
    from fno.claims.io import global_claims_root, list_claim_keys

    try:
        return [
            {"key": key, "state": None}
            for key in list_claim_keys("node:", global_claims_root())
        ]
    except Exception as exc:  # noqa: BLE001 - identify the failed source
        raise ObserverReadError(f"claims unreadable: {exc}") from exc


def read_planned_unclaimed_from_entries(
    entries: list[dict],
    *,
    project: str | None = None,
    mission: str | None = None,
    roadmap_id: str | None = None,
    parent: str | None = None,
) -> dict:
    from fno.graph.statuses import live_worked_node_ids

    try:
        claims = read_claim_snapshot()
        # A node whose worker outlived its claim TTL must not read as
        # offerable here either: fold the worked overlay in as synthetic
        # claims, and refuse on an unreadable roster rather than offer nodes
        # whose liveness could not be checked.
        worked = live_worked_node_ids(strict=True, entries=entries)
    except ValueError as exc:
        raise ObserverReadError(str(exc)) from exc
    except Exception as exc:  # noqa: BLE001 - unknown liveness refuses the offer
        raise ObserverReadError(f"worked overlay unreadable: {exc}") from exc
    claims = [
        *claims,
        *( {"key": f"node:{node_id}", "state": "live-worker"} for node_id in worked ),
    ]
    try:
        return classify_planned_unclaimed(
            entries,
            claims,
            project=project,
            mission=mission,
            roadmap_id=roadmap_id,
            parent=parent,
        )
    except ValueError as exc:
        raise ObserverReadError(str(exc)) from exc


def build_selection_divergence_event(
    *,
    node_id: str,
    selector_command: str,
    observer_command: str = OBSERVER_COMMAND,
    scope: str,
    selector_entries_scanned: int,
    observer_entries_scanned: int,
) -> dict:
    from fno.events import _build

    return _build(
        "dispatch_selection_diverged",
        "backlog",
        {
            "node_id": node_id,
            "selector_command": selector_command,
            "observer_command": observer_command,
            "scope": scope,
            "selector_entries_scanned": selector_entries_scanned,
            "observer_entries_scanned": observer_entries_scanned,
        },
    )
