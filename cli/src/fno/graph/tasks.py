"""Task-grain rows under a backlog node (epic x-09d7, group 3).

A node's graph entry may carry a ``tasks`` key: one row per task the bound
plan's ``## Execution Strategy`` declares::

    {"id": "1.1", "status": "pending", "owner": None}

The plan stays the source of task identity; the row carries only status and
owner, so a second session can read who is on a task before starting it. Rows
are DERIVED (never hand-authored): ``ensure_task_rows`` adds a pending row for
every plan task id not yet present and never overwrites an existing row.

The claim that guards the ``pending -> in_progress`` transition lives in
:mod:`fno.claims.tasks`; the transition itself is owned by the ``task``
sub-typer registered on the backlog CLI.
"""
from __future__ import annotations

from pathlib import Path

TASK_STATUSES = ("pending", "in_progress", "done")


def derive_task_ids(plan_path: Path) -> list[str]:
    """Task ids from the plan's ``## Execution Strategy`` (canonical parser).

    Delegates to ``fno.plan._doc.load_plan`` + ``fno.plan.brief.parse_execution_strategy``
    - the same sequence ``skills/execute/orchestrator.py`` ``load_plan_strategy``
    uses - so the graph rows and the orchestrator can never disagree about what
    a task id is. A plan with no Execution Strategy section yields ``[]``
    (nothing to derive), matching ``load_plan_strategy``'s degrade.
    """
    from fno.plan._doc import load_plan
    from fno.plan.brief import parse_execution_strategy

    doc = load_plan(Path(plan_path))
    body = doc.get_section("Execution Strategy")
    if body is None:
        return []
    raw = parse_execution_strategy(body)
    return [t["id"] for t in raw.get("tasks", []) if t.get("id")]


def ensure_task_rows(entry: dict, plan_path: Path) -> list[dict]:
    """Materialize ``entry['tasks']`` rows for plan task ids not yet present.

    Idempotent and never destructive: existing rows are kept verbatim (an
    ``in_progress`` row stays ``in_progress``), new plan ids gain
    ``{'id', 'status': 'pending', 'owner': None}``, and rows whose id left the
    plan are left alone (a replan mid-run must not drop a live claim's row).
    Returns the entry's row list.
    """
    rows = entry.get("tasks")
    rows = [r for r in rows if isinstance(r, dict)] if isinstance(rows, list) else []
    known = {r.get("id") for r in rows}
    for task_id in derive_task_ids(Path(plan_path)):
        if task_id not in known:
            rows.append({"id": task_id, "status": "pending", "owner": None})
    entry["tasks"] = rows
    return rows


__all__ = ["TASK_STATUSES", "derive_task_ids", "ensure_task_rows"]
