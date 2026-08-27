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


def ensure_task_rows(
    entry: dict, plan_path: Path, task_ids: "list[str] | None" = None
) -> list[dict]:
    """Materialize ``entry['tasks']`` rows for plan task ids not yet present.

    Idempotent and never destructive: existing rows are kept verbatim (an
    ``in_progress`` row stays ``in_progress``), new plan ids gain
    ``{'id', 'status': 'pending', 'owner': None}``, and rows whose id left the
    plan are left alone (a replan mid-run must not drop a live claim's row).
    Returns the READABLE rows: a row that is not a dict stays in
    ``entry['tasks']`` but is never handed to a caller that would read it.

    Pass ``task_ids`` when the caller already derived them. Re-parsing the
    plan here happens INSIDE the graph flock, where a parse error escapes as a
    traceback instead of the named refusal every other task-verb failure gets.
    """
    raw = entry.get("tasks")
    if raw is not None and not isinstance(raw, list):
        # Not a list at all: corrupt, and replacing it with a fresh list would
        # write the corruption away. Touch nothing and materialize nothing.
        return []
    raw = raw or []
    # Keep what cannot be read rather than dropping it: this list is written
    # back over entry["tasks"], so filtering non-dict rows out here DELETES
    # them from graph.json. read_graph and locked_mutate_graph both preserve
    # what they cannot migrate; a row writer skipping a row it cannot parse is
    # the containment, not a silent prune.
    rows = list(raw)
    known = {r.get("id") for r in rows if isinstance(r, dict)}
    ids = derive_task_ids(Path(plan_path)) if task_ids is None else task_ids
    for task_id in ids:
        if task_id not in known:
            rows.append({"id": task_id, "status": "pending", "owner": None})
            # Without this the SAME id twice in one plan yields two rows, and
            # every writer `break`s on the first - orphaning the second at
            # pending forever. Reachable: PyYAML reads `id: 2.10` as 2.1, so
            # 2.1 and 2.10 collide.
            known.add(task_id)
    entry["tasks"] = rows
    return [r for r in rows if isinstance(r, dict)]


__all__ = ["TASK_STATUSES", "derive_task_ids", "ensure_task_rows"]
