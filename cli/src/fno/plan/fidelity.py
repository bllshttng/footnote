"""Plan fidelity as a gate decision (x-cbab).

The fold (`fno.scoreboard.fold.build_plan_fidelity`) computes the planned-to-
delivered join for TELEMETRY: an unjoined plan is unmeasurable, never 0%. That
is exactly wrong for a gate, where an unjoined planned row means a declared
deliverable never shipped. The inversion is ONE parameter on the same join
(``unmeasurable='refuse'``), not a second implementation - so the scoreboard and
the gate cannot disagree about the same PR.

This module owns the carveout waiver - the one thing the fold does not. Each
unjoined planned row needs a covering carveout or the gate refuses. A PR-body
sentence is not a carveout. Both gate readers route through the single decision
here: ``_merge.py`` imports :func:`compute_plan_fidelity`; ``loopcheck.rs``
shells ``fno plan fidelity --json``.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Optional

__all__ = ["fidelity_refusal", "compute_plan_fidelity"]


def fidelity_refusal(*, unjoined_rows: list[dict], carveouts: list[dict]) -> dict:
    """The carveout-aware gate decision over the fidelity join's shortfall.

    Each unjoined planned row needs a covering carveout, or the gate refuses.
    Coverage is count-based: carveouts carry no per-deliverable reference field
    today, so a 1:1 match is not mechanically checkable, and a run that files C
    carveouts covers C shortfalls. ``refused`` is the load-bearing field.

    Pure (no I/O): takes the join's shortfall and the caller-scoped carveouts,
    returns the decision. This is the unit-testable heart of AC5.
    """
    shortfall = len(unjoined_rows)
    covering = len(carveouts)
    covered = shortfall > 0 and covering >= shortfall
    refused = shortfall > 0 and not covered
    return {
        "refused": refused,
        "shortfall": shortfall,
        "carveouts": covering,
        "covered": covered,
        "reason": (
            f"{shortfall} planned deliverable(s) unjoined with only "
            f"{covering} covering carveout(s); a PR-body sentence is not a carveout"
            if refused
            else None
        ),
        "unjoined": unjoined_rows,
    }


def compute_plan_fidelity(
    *,
    plan_path: str,
    repo_root: Optional[str] = None,
    since_days: int = 180,
    now: Any = None,
) -> dict:
    """Assemble the gate decision for one plan and return the shape both readers
    consume (``refused`` is load-bearing).

    Loads the ledger, runs the join scoped to ``plan_path``, reads carveouts
    filed by the plan's sessions, and applies :func:`fidelity_refusal`. Fail-closed
    on an unreadable plan (the inversion): telemetry degrades an unreadable plan
    to n/a, the gate refuses - inverting this ships a gate that passes hardest
    exactly when the plan is missing.
    """
    from datetime import datetime
    from pathlib import Path

    from fno import paths as _paths
    from fno.scoreboard.fold import _plan_key, build_plan_fidelity, load_ledger_rows

    now = now or datetime.now()

    plan_file = Path(plan_path.split("#", 1)[0])
    if not plan_file.is_file():
        return _refused(
            f"plan unreadable ({plan_path}); a gate degrades to refuse, not n/a"
        )

    try:
        rows = load_ledger_rows(_paths.ledger_json())
    except Exception:  # noqa: BLE001 - a missing ledger is no planned rows, not a crash
        rows = []
    graph_nodes = _load_graph_nodes()

    pf = build_plan_fidelity(
        rows, graph_nodes, since_days=since_days, now=now, unmeasurable="refuse"
    )
    if pf.get("state") != "ok":
        # No planned rows in window: nothing to measure, nothing to refuse.
        return _passthrough(planned=0, delivered=0)

    target = _plan_key(plan_path)
    plan_rows = [r for r in pf["results"] if _plan_key(r.get("plan_path")) == target]
    unjoined = [r for r in plan_rows if r.get("status") == "unjoined"]
    planned = len(plan_rows)
    delivered = planned - len(unjoined)

    session_ids = {r.get("session_id") for r in plan_rows if r.get("session_id")}
    carveouts = _read_covering_carveouts(repo_root, session_ids)

    decision = fidelity_refusal(unjoined_rows=unjoined, carveouts=carveouts)
    decision["planned"] = planned
    decision["delivered"] = delivered
    decision["plan_path"] = plan_path
    return decision


def _refused(reason: str) -> dict:
    return {
        "refused": True, "reason": reason, "planned": 0, "delivered": 0,
        "shortfall": 0, "carveouts": 0, "covered": False, "unjoined": [],
    }


def _passthrough(*, planned: int, delivered: int) -> dict:
    return {
        "refused": False, "reason": None, "planned": planned, "delivered": delivered,
        "shortfall": 0, "carveouts": 0, "covered": True, "unjoined": [],
    }


def _load_graph_nodes() -> list[dict]:
    try:
        from fno.graph.load import load_graph

        graph = load_graph(_graph_json_path())
        return graph if isinstance(graph, list) else []
    except Exception:  # noqa: BLE001 - graph optional; the join tolerates empty
        return []


def _read_covering_carveouts(repo_root: Optional[str], session_ids: set) -> list[dict]:
    if not session_ids:
        return []
    try:
        from fno import paths as _paths
        from fno.carveout.core import read_carveouts

        root = Path(repo_root) if repo_root else _paths.resolve_repo_root()
        return read_carveouts(root, session_ids=list(session_ids))
    except Exception:  # noqa: BLE001 - unreadable carveouts fail OPEN on coverage
        # only because the unreadable-plan case already refused above; a carveout
        # read failure must not be the hole that ships an uncovered shortfall.
        return []


def _graph_json_path():
    from fno import paths as _paths

    return _paths.graph_json()
