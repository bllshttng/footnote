"""Design-stage probe: is a node's linked plan still pre-blueprint?

The lifecycle ladder (idea -> design -> ready -> in-progress -> in-review ->
done) is already derived twice in this codebase: ``recompute_statuses`` owns
the graph vocabulary and ``fno.plan._status`` owns the plan-frontmatter
vocabulary (``design -> ready -> in_progress -> shipped``). The only rung the
graph side could not see is ``design`` - a node whose plan doc exists but is
still a design doc, not a blueprint.

Derived per read rather than persisted into ``_status``. A plan doc is external
mutable state that ``/blueprint`` rewrites WITHOUT touching the graph, and
``read_graph`` does not recompute ``_status`` (only ``locked_mutate_graph``
does), so a persisted ``design`` would never re-arm once the blueprint landed -
the node would starve invisibly forever. Same shape as
``statuses.live_claimed_node_ids``, which overlays the claim lockfile for the
same reason.
"""
from __future__ import annotations


def is_design_stage(plan_path: object) -> bool:
    """True only when the linked plan's frontmatter says ``status: design``.

    Positive evidence only: an unreadable, frontmatter-less, or
    differently-stamped plan reads False and the node stays armed. Failing
    OPEN is deliberate and load-bearing - plans live in a symlinked vault, so
    demoting on a read failure would quarantine the entire backlog the moment
    that vault is unmounted. Mirrors the fail-open contract of
    ``selection_guards``, its only selection-path caller.

    Keys on frontmatter rather than a ``## Execution Strategy`` heading because
    `/blueprint quick` deliberately omits that heading (blueprint SKILL.md),
    so the heading would misread every quick-plan as unfinished.
    """
    if not isinstance(plan_path, str) or not plan_path:
        return False
    from fno.graph._intake import _read_plan_frontmatter

    raw = _read_plan_frontmatter(plan_path).get("status")
    return str(raw if raw is not None else "").strip().strip("'\"").lower() == "design"
