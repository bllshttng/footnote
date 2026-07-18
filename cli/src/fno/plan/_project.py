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
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from fno.plan._stamp import read_plan_file, write_plan_file
from fno.plan._status import project_plan_status
from fno.plan._rollup import ROLLUP_KEYS, compute_rollup, compute_waves

# Graph-authoritative fields mirrored into frontmatter. `type` is usually
# already present; the projection keeps it in sync with the node. `parent_slug`
# is not a native node field - the converger injects it (see project_graph_nodes).
MIRROR_KEYS: tuple[str, ...] = (
    "priority",
    "type",
    "blocked_by",
    "tags",
    "project",
    "size",
    "parent",
    "parent_slug",
)

# Mirror keys that are always lists; an empty list is meaningful (it clears a
# stale mirror), so they bypass the None-skip path a scalar takes.
LIST_MIRROR_KEYS: frozenset[str] = frozenset({"blocked_by", "tags"})

# Mirror keys whose graph value can legitimately be cleared to None (a de-orphan
# `--parent null`, a `--size null`). For these, an explicit None means "clear the
# stale doc mirror", not "skip" - otherwise the doc keeps the old parent/size
# after the graph dropped it. parent_slug is tied to parent: the converger sets
# it to None whenever parent is null/dangling so it clears in lockstep.
CLEARABLE_KEYS: frozenset[str] = frozenset({"size", "parent", "parent_slug"})


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
        if key in LIST_MIRROR_KEYS:
            # Always a list; empty list is meaningful (clears a stale mirror).
            # A non-list value is corrupt input - skip, don't crash.
            if not isinstance(value, list):
                continue
        elif value is None:
            # A clearable key set to None means the graph dropped its value
            # (de-orphan / --size null): remove the stale doc mirror if present.
            # Any other None is a partial dict and must never clobber the doc.
            if key in CLEARABLE_KEYS and key in fields:
                del fields[key]
                changed = True
            continue
        if fields.get(key) != value:
            fields[key] = value
            changed = True

    # Epic rollup counters (x-6c2b): epic-only, injected by the converger. A key
    # present => write it; absent (every leaf doc) => skip, so leaf frontmatter
    # stays clean. The frontmatter reader returns every scalar as a str, so
    # compare (and store) the str form or an int counter re-writes forever.
    # `children_total: 2` still serializes bareword, so Obsidian reads a number.
    # Derived epic fields: rollup counters, the epic `waves` summary, and a
    # child's `wave` stratum (x-6c2b). All computed views, repainted every
    # projection, never hand-set. The converger passes an explicit None when a
    # key no longer applies (a node demoted out of epic-hood, a child orphaned
    # off its epic) so the stale value is CLEARED, not left to rot - the
    # graph-authoritative contract (codex). Present str value => write; None =>
    # delete if present; absent => leave alone (a bare direct caller never
    # clears).
    for key in (*ROLLUP_KEYS, "waves", "wave"):
        if key not in node:
            continue
        if node[key] is None:
            if key in fields:
                del fields[key]
                changed = True
            continue
        value = str(node[key])
        if fields.get(key) != value:
            fields[key] = value
            changed = True

    # Status projection (x-f34f): map the graph derived `_status` onto the plan,
    # forward-only. Kept out of MIRROR_KEYS because it is a mapped, monotonic
    # write (not a straight mirror) and stamps done_at on the terminal write.
    graph_status = node.get("_status")
    if graph_status:
        current_status = fields.get("status")
        projected = project_plan_status(current_status, graph_status)
        if projected is not None and current_status != projected:
            fields["status"] = projected
            changed = True
            if projected == "done" and not fields.get("done_at"):
                fields["done_at"] = datetime.now(timezone.utc).strftime(
                    "%Y-%m-%dT%H:%M:%SZ"
                )

    if changed:
        write_plan_file(target, fields, rest)
    return changed


def project_graph_nodes(
    entries: list[dict[str, Any]],
    node_ids: list[str],
    root: str | None = None,
) -> int:
    """Project each named node's mirror fields onto its linked plan.

    The shared converger primitive behind both the instrumented mutating verbs
    and the `fno plan sync` sweep: for each id, find the node in ``entries``,
    resolve+absolutize its ``plan_path`` (against ``root``), skip absent files,
    inject the parent's slug, and call ``project_node_to_plan``. Best-effort and
    per-node isolated - one unreadable doc never aborts the batch. Returns the
    count of docs rewritten.

    ``entries`` is the already-read graph (this module never imports
    ``graph.store`` - Locked Decision 1). ``root`` is resolved lazily only when a
    relative ``plan_path`` is first seen.
    """
    ids = [i for i in dict.fromkeys(node_ids) if i]
    if not ids:
        return 0
    from fno.graph._intake import _find_node, repo_root

    # Parent-repaint hop (x-6c2b wave 2): a child mutation must also repaint its
    # parent epic's doc so its rollup counters stay live. Walk one level up.
    ids = _expand_repaint_targets(entries, ids)

    slug_by_id = {
        n.get("id"): n.get("slug") for n in entries if isinstance(n, dict)
    }
    rewritten = 0
    for nid in ids:
        try:
            node = _find_node(entries, nid)
            if not node or not node.get("plan_path"):
                continue
            p = Path(node["plan_path"])
            if not p.is_absolute():
                if root is None:
                    root = repo_root()
                p = Path(root) / p
            if not p.is_file():
                continue
            augmented = _with_parent_slug(node, slug_by_id)
            if node.get("type") == "epic":
                augmented.update(compute_rollup(node["id"], entries))
                # Epic-altitude wave summary: max child stratum + 1 (0 when
                # childless). Derived from intra-epic blocked_by edges (AC4).
                _, max_wave = compute_waves(node["id"], entries)
                augmented["waves"] = max_wave + 1
            else:
                # Not an epic: clear any stale epic-only derived fields a prior
                # projection left behind (a --type feature demotion) so the doc
                # stays graph-authoritative (codex).
                for k in (*ROLLUP_KEYS, "waves"):
                    augmented[k] = None
            # A node's own stratum within its parent epic (mission or plain);
            # None (=> cleared) when it has no epic parent, e.g. after --parent
            # null orphans it off the epic.
            parent_id = node.get("parent")
            wave_val: int | None = None
            if parent_id:
                parent = _find_node(entries, parent_id)
                if parent is not None and parent.get("type") == "epic":
                    wave_map, _ = compute_waves(parent_id, entries)
                    wave_val = wave_map.get(node.get("id"))
            augmented["wave"] = wave_val
            if project_node_to_plan(augmented, p):
                rewritten += 1
        except Exception as e:  # noqa: BLE001 - per-node best-effort
            sys.stderr.write(f"warning: plan projection failed for {nid}: {e}\n")
    return rewritten


def _expand_repaint_targets(
    entries: list[dict[str, Any]], ids: list[str]
) -> list[str]:
    """Add each projected node's ancestors AND its siblings so a child transition
    repaints the epic + mission rollup (walk up to the mission -> epic -> leaf
    cap, two hops) and every sibling's derived wave (x-6c2b AC4: one child's
    blocked_by edit can restratify the whole epic). Order-preserving, deduped. A
    missing/dangling parent just stops the walk - a doc-less node is a later no-op.
    """
    by_id = {
        n.get("id"): n for n in entries if isinstance(n, dict) and n.get("id")
    }
    children_by_parent: dict[str, list[str]] = {}
    for n in entries:
        if isinstance(n, dict):
            nid = n.get("id")
            pid = n.get("parent")
            if isinstance(nid, str) and isinstance(pid, str):
                children_by_parent.setdefault(pid, []).append(nid)
    out = list(ids)
    seen = set(ids)

    def _add(nid: str) -> None:
        if nid not in seen:
            seen.add(nid)
            out.append(nid)

    for nid in ids:
        cur = by_id.get(nid)
        hops = 0
        while cur and hops < 2:
            parent = cur.get("parent")
            if not parent:
                break
            _add(parent)
            # Siblings share the IMMEDIATE parent's wave stratification, so an
            # edge change on one child shifts theirs - repaint them. Only at
            # hops == 0: at the mission hop the "siblings" are other epics that
            # share neither strata nor rollup with the moved leaf, so expanding
            # there is pure over-repaint (gemini).
            if hops == 0:
                for sib in children_by_parent.get(parent, ()):
                    _add(sib)
            cur = by_id.get(parent)
            hops += 1
    return out


def _with_parent_slug(
    node: dict[str, Any], slug_by_id: dict[Any, Any]
) -> dict[str, Any]:
    """Return a shallow copy of ``node`` with ``parent_slug`` tied to ``parent``.

    Never mutates the shared ``entries`` element. A resolvable parent sets the
    slug; a null, absent, or dangling parent sets ``parent_slug`` to None so a
    stale slug mirror CLEARS in lockstep with the parent (a dangling parent still
    mirrors its raw id but never a wrong slug).
    """
    parent_id = node.get("parent")
    copy = dict(node)
    copy["parent_slug"] = slug_by_id.get(parent_id) if parent_id else None
    return copy
