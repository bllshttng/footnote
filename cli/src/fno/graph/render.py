"""Graph Kanban rendering for Obsidian graph.md.

Public API:
    render_graph_md(entries, path) -> None
"""
from __future__ import annotations

import os
import tempfile
from pathlib import Path

from fno.graph._constants import GRAPH_MD, PRIORITY_ORDER, _rank_band

# Canonical Kanban column order, left to right. Single source of truth for both
# renderers: render_graph_md (below) and render_html.COLUMNS import this, so the
# column set + order can never drift between the markdown board and the HTML
# board. Now leads (genuine today-work), Triage holds the awaiting-ack queue,
# Done is terminal.
KANBAN_COLUMNS = ("Now", "Next", "Later", "Triage", "Done")

# Lane label for a node with no project. Shared single source of truth for both
# renderers (render_html imports it) so the swimlane grouping label can never
# drift between the markdown board and the HTML board.
UNSCOPED_LABEL = "(unscoped)"


def _graph_sort_key(e: dict) -> tuple:
    """Sort key for graph entries: priority (p0 first), then creation time.

    ``created_at`` uses ``or ""`` rather than a ``.get`` default so an explicit
    ``null`` (None) coerces to "" too. Without it, two same-priority nodes - one
    with ``created_at: null`` and one with a real timestamp - raise a TypeError
    ("'<' not supported between NoneType and str") mid-sort. _apply_graph_defaults
    never backfills created_at, so null values reach here from real graph.json rows.
    """
    return (PRIORITY_ORDER.get(e.get("priority", "p2"), 2), e.get("created_at") or "")


def _project_key(entry: dict) -> str:
    """Normalize a node's project for swimlane grouping.

    Missing/empty/whitespace -> UNSCOPED_LABEL. Shared by both renderers
    (hoisted from render_html so render.py can reuse it for the lane key).
    """
    proj = entry.get("project")
    if isinstance(proj, str) and proj.strip():
        return proj
    return UNSCOPED_LABEL


def _lane_order_key(project: str) -> tuple:
    """Order lanes within a column: named projects alphabetical, unscoped last."""
    return (project == UNSCOPED_LABEL, project)


def _lane_sort_key(entry: dict) -> tuple:
    """Shared within-column ordering key, consumed by both renderers.

    Orders a column's cards by ``(project_lane, rank_band, priority,
    created_at)``: cluster by project, then ranked-before-unranked
    (ascending rank), then today's ``_graph_sort_key`` fallback. Rank is
    therefore scoped per ``(column, project)`` lane.
    """
    return (
        _lane_order_key(_project_key(entry)),
        _rank_band(entry),
        _graph_sort_key(entry),
    )


def _kanban_column(entry: dict) -> str | None:
    """Return the kanban column for a graph entry, or None to exclude.

    Mapping (intent-based, with claimed override):

    - roadmap nodes -> excluded
    - completed_at set -> Done
    - deferred / superseded -> excluded (off-board until reactivated)
    - claimed (a session is on it) -> Now (overrides priority)
    - queued (awaiting human ack) -> Triage (overrides priority, below claimed)
    - else by priority:
        p0 / p1 -> Now    (today-ish)
        p2      -> Next   (this sprint / this week)
        p3      -> Later  (long-tail)

    Blocked and idea statuses are NOT special-cased here - they ride
    their priority into the same column as everything else. Renderers
    are expected to surface the side state as a card-level visual flag.
    """
    if entry.get("type") == "roadmap":
        return None
    if entry.get("completed_at"):
        return "Done"
    status = entry.get("_status", "ready")
    if status in ("deferred", "superseded"):
        return None
    if status == "claimed":
        return "Now"
    # Queued is orthogonal to _status (the field stays set across blocked,
    # idea, etc.). It routes to the Triage lane - a queued node is "awaiting
    # human ack" (via `fno backlog pick`), not active work, so it must NOT
    # inflate Now. Still below claimed: a claimed node is actively in progress
    # and stays in Now even when also queued.
    if entry.get("queued_at"):
        return "Triage"
    priority = entry.get("priority") or "p2"
    if priority in ("p0", "p1"):
        return "Now"
    if priority == "p3":
        return "Later"
    return "Next"


def _kanban_card(entry: dict, id_to_entry: dict[str, dict]) -> str:
    """Format a single kanban card line for an entry."""
    title = (entry.get("title") or "").replace("\n", " ").strip() or "(untitled)"
    eid = entry.get("id", "?")
    priority = entry.get("priority") or "p2"
    is_done = bool(entry.get("completed_at"))
    is_deferred = bool(entry.get("deferred_at")) and not is_done
    marker = "[x]" if is_done else "[ ]"

    # Project lane label leads the metadata tail so per-project clusters are
    # legible (the Obsidian Kanban plugin is column-only, so a per-card label
    # plus clustered sort order is the honest ceiling for swimlanes - ab-95a4a479).
    project = _project_key(entry)
    header = f"- {marker} **{title}** `{eid}` · {project} · {priority}"
    body_lines: list[str] = []

    plan_path = entry.get("plan_path")
    if plan_path:
        body_lines.append(f"  {plan_path}")

    blockers = [b for b in entry.get("blocked_by", []) if isinstance(b, str)]
    if blockers and not is_done:
        blocker_titles = []
        for bid in blockers:
            blocker = id_to_entry.get(bid)
            if blocker and not blocker.get("completed_at"):
                blocker_titles.append(f"{bid} ({(blocker.get('title') or '?')[:40]})")
        if blocker_titles:
            body_lines.append(f"  blocked by: {', '.join(blocker_titles)}")

    if is_deferred:
        reason = (entry.get("deferred_reason") or "").strip()
        body_lines.append(f"  deferred: {reason}" if reason else "  deferred")

    pr_url = entry.get("pr_url")
    if pr_url and is_done:
        body_lines.append(f"  {pr_url}")
    if is_done:
        for extra in entry.get("additional_prs") or []:
            if not isinstance(extra, dict):
                continue
            extra_url = extra.get("url")
            extra_num = extra.get("number")
            label = extra_url or (f"#{extra_num}" if extra_num is not None else None)
            if label is None:
                continue
            note = (extra.get("note") or "").strip()
            body_lines.append(f"  {label}{' - ' + note if note else ''}")

    if body_lines:
        return header + "\n" + "\n".join(body_lines)
    return header


def render_graph_md(
    entries: list[dict], path: Path = GRAPH_MD, *, obsidian: bool = True
) -> None:
    """Render graph.json entries as a Kanban-style markdown board.

    Columns: Now (claimed / p0-p1), Next (p2), Later (p3),
    Triage (queued, awaiting human ack), Done (completed). Within each
    non-Done column, cards sort by the shared lane key (project, then
    ranked-before-unranked, then priority, then created_at) so per-project
    clusters are contiguous. Done sorts by completed_at (capped at 10).

    ``obsidian`` (default True, preserving prior behavior) controls the
    Obsidian Kanban plugin scaffolding: the ``kanban-plugin: board``
    frontmatter and the trailing ``%% kanban:settings`` block. When False
    (no Obsidian vault), both are omitted - they are inert noise outside
    Obsidian - and the file is plain column markdown. This function stays
    pure: the caller passes the flag rather than reading settings here.

    The caller (locked_mutate_graph) catches OSError only, so IO failures
    don't crash a successful JSON write. Programmer bugs like KeyError
    or TypeError propagate so they're visible in development.
    """
    id_to_entry = {e["id"]: e for e in entries if isinstance(e.get("id"), str)}

    columns: dict[str, list[dict]] = {col: [] for col in KANBAN_COLUMNS}
    for entry in entries:
        col = _kanban_column(entry)
        if col is None:
            continue
        columns[col].append(entry)

    for col, items in columns.items():
        if col == "Done":
            # `or ""` (not a .get default) so an explicit completed_at: null
            # coerces to "" too - same None-vs-str TypeError class as _graph_sort_key.
            items.sort(key=lambda e: e.get("completed_at") or "", reverse=True)
            del items[10:]
        else:
            items.sort(key=_lane_sort_key)

    lines: list[str] = ["---", "kanban-plugin: board", "---", ""] if obsidian else []
    for col in KANBAN_COLUMNS:
        lines.append(f"## {col}")
        lines.append("")
        if not columns[col]:
            lines.append("")
            continue
        for entry in columns[col]:
            lines.append(_kanban_card(entry, id_to_entry))
            lines.append("")

    if obsidian:
        lines.append("***")
        lines.append("")
        lines.append("%% kanban:settings")
        lines.append("```")
        lines.append('{"kanban-plugin":"board"}')
        lines.append("```")
        lines.append("%%")
    content = "\n".join(lines) + "\n"

    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_fd, tmp_path = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
    try:
        with os.fdopen(tmp_fd, "w", encoding="utf-8") as f:
            f.write(content)
        os.replace(tmp_path, str(path))
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
