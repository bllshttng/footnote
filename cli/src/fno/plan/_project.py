"""Project graph-node navigation fields onto a plan's frontmatter.

One-way graph->doc mirror: the graph is the authority, the plan frontmatter
carries a PROJECTION so the Obsidian Bases can order "Next up" by priority and
show blockers without a second lookup. Written only by fno verbs (intake,
`backlog update`); never read back into the graph here (`size` flows doc->graph
at intake, a separate reverse path in `_intake`).

Reuses `_stamp`'s byte-preserving frontmatter reader/writer so the projection
never reorders keys or reformats opaque blocks like `kill_criteria`.
"""
from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

from fno.plan._stamp import read_plan_file, write_plan_file

# Graph-authoritative fields mirrored into frontmatter. `type` is usually
# already present; the projection keeps it in sync with the node.
MIRROR_KEYS: tuple[str, ...] = ("priority", "type", "blocked_by", "project")


def project_node_to_plan(node: dict[str, Any], plan_path: Path) -> bool:
    """Upsert the mirror fields from ``node`` into ``plan_path``'s frontmatter.

    Returns True if the file was rewritten (a mirrored value changed), False on
    a no-op or when the plan file is missing/unreadable. Never raises: a graph
    mutation must not fail because its projected doc is absent or unreadable
    (warns to stderr instead).
    """
    try:
        target, fields, rest = read_plan_file(plan_path)
    except (FileNotFoundError, OSError, ValueError) as exc:
        sys.stderr.write(
            f"warning: plan projection skipped, cannot read {plan_path}: {exc}\n"
        )
        return False

    changed = False
    for key in MIRROR_KEYS:
        if key not in node:
            continue
        value = node[key]
        if key == "blocked_by":
            # Always a list; empty list is meaningful (clears a stale blocker
            # mirror). A non-list value is corrupt input - skip, don't crash.
            if not isinstance(value, list):
                continue
        elif value is None:
            # Never overwrite a real frontmatter value with a null scalar.
            continue
        if fields.get(key) != value:
            fields[key] = value
            changed = True

    if changed:
        write_plan_file(target, fields, rest)
    return changed
