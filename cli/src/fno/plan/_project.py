"""Project graph-node navigation fields onto a plan's frontmatter.

One-way graph->doc mirror: the graph is the authority, the plan frontmatter
carries a PROJECTION so the Obsidian Bases can order "Next up" by priority and
show blockers without a second lookup. Written only by fno verbs (intake,
`backlog update`); never read back into the graph here (`size` and `type` flow
doc->graph at intake, a separate reverse path in `_intake`).
"""
from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

from fno import paths
from fno.graph import store


def plan_docs(op: str, **params: Any) -> "dict | None":
    """One keeper plan_docs call. Never raises: an unreachable keeper warns, returns None."""
    for key in ("mirror_keys_for", "clear_keys_for"):
        if params.get(key):
            params[key] = {"id": params[key][0], "keys": sorted(params[key][1])}
    params.update(op=op, cwd=str(Path.cwd()), events_path=str(paths.project_events_json()))
    try:
        result = store._client_for(store.GRAPH_JSON).request("plan_docs", params)
    except (store.StoreUnavailable, RuntimeError) as exc:
        sys.stderr.write(f"warning: plan-doc writer unreachable ({exc}); run `fno doctor`\n")
        return None
    for line in result.get("warnings") or []:
        sys.stderr.write(f"{line}\n")
    return result


def project_graph_nodes(
    entries: list[dict[str, Any]],
    node_ids: list[str],
    root: str | None = None,
    *,
    mirror_keys_for: tuple[str, frozenset[str]] | None = None,
    force_status_off_terminal_for: str | None = None,
    clear_keys_for: tuple[str, frozenset[str]] | None = None,
) -> int:
    """Project each named node's mirror fields onto its linked plan; returns docs rewritten."""
    ids = [i for i in dict.fromkeys(node_ids) if i]
    if not ids:
        return 0
    result = plan_docs("project", ids=ids, root=root, mirror_keys_for=mirror_keys_for,
                       force_status_off_terminal_for=force_status_off_terminal_for, clear_keys_for=clear_keys_for)
    return int(result["rewritten"]) if result else 0
