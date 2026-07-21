"""Design-stage probe: is a node's linked plan still pre-blueprint?

The lifecycle ladder (idea -> design -> ready -> in-progress -> in-review ->
done) is already derived twice in this codebase: ``recompute_statuses`` owns
the graph vocabulary and ``fno.plan._status`` owns the plan-frontmatter
vocabulary (``design -> ready -> in_progress -> shipped``). The only rung the
graph side could not see is ``design`` - a node whose plan doc exists but is
still a design doc, not a blueprint.

Derived per read rather than persisted into ``status``. A plan doc is external
mutable state that ``/blueprint`` rewrites WITHOUT touching the graph, and
``read_graph`` does not recompute ``status`` (only ``locked_mutate_graph``
does), so a persisted ``design`` would never re-arm once the blueprint landed -
the node would starve invisibly forever. Same shape as
``statuses.live_claimed_node_ids``, which overlays the claim lockfile for the
same reason.
"""
from __future__ import annotations

import os
from typing import Optional


def resolve_plan_probe(entry: dict) -> Optional[str]:
    """Filesystem path for a node's plan doc, or None when it has no usable one.

    Resolves the way the node itself would: strip a ``#anchor`` fragment,
    expand ``~``, and resolve a repo-relative path against the NODE's own
    ``cwd`` rather than the calling process's. The daemon selects across
    projects, so probing a foreign node's relative path against the current
    process cwd would silently find nothing - on the live graph that is the
    majority of linked plans, not an edge case.
    """
    if not isinstance(entry, dict):
        return None
    plan_path = entry.get("plan_path")
    if not isinstance(plan_path, str) or not plan_path:
        return None
    probe = os.path.expanduser(plan_path.split("#", 1)[0])
    if not probe:
        return None
    if not os.path.isabs(probe):
        cwd = entry.get("cwd")
        if not (isinstance(cwd, str) and cwd):
            # No anchor to resolve against. Returning the relative path would
            # silently resolve it against the CALLING process's cwd, where a
            # coincidentally-matching local doc could design-gate an unrelated
            # node. Refuse to guess and let the caller fail open instead.
            return None
        probe = os.path.join(cwd, probe)
    return probe


def is_design_stage(entry: object) -> bool:
    """True only when the node's linked plan says ``status: design``.

    Takes the whole entry, not a bare path, because resolving the path needs
    the node's ``cwd`` (see ``resolve_plan_probe``).

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
    if not isinstance(entry, dict):
        return False
    probe = resolve_plan_probe(entry)
    if not probe:
        return False
    from fno.graph._intake import _read_plan_frontmatter

    try:
        raw = _read_plan_frontmatter(probe).get("status")
    except Exception:  # noqa: BLE001 - fail OPEN; see the fail-open note above
        # `_read_plan_frontmatter` now absorbs the read errors this was written
        # for (including UnicodeDecodeError from a binary file at the plan path).
        # Kept as a belt-and-braces net because `detect_stale_ready` has no outer
        # catch, so any future escaping read error would abort a `maintain` run.
        return False
    return str(raw if raw is not None else "").strip().strip("'\"").lower() == "design"
