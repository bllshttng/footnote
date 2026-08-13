"""Graph Kanban rendering for Obsidian graph.md.

Public API:
    render_graph_md(entries, path) -> None
"""
from __future__ import annotations

import os
import tempfile
from collections.abc import Collection
from pathlib import Path

from fno.graph._constants import GRAPH_MD, PRIORITY_ORDER, _rank_band as _rank_band
from fno.graph._intake import (
    UNSCOPED_LABEL as UNSCOPED_LABEL,
    _project_key,
    make_effective_priority,
    make_selection_sort_key,
)
from fno.graph.statuses import live_claimed_node_ids

# Canonical Kanban column order, left to right. Single source of truth for both
# renderers: render_graph_md (below) and render_html.COLUMNS import this, so the
# column set + order can never drift between the markdown board and the HTML
# board. Now leads (genuine today-work), Triage holds the awaiting-ack queue,
# Done is terminal.
KANBAN_COLUMNS = ("Now", "Next", "Later", "Triage", "Done")

def _graph_sort_key(e: dict) -> tuple:
    """Sort key for graph entries: priority (p0 first), then creation time.

    ``created_at`` uses ``or ""`` rather than a ``.get`` default so an explicit
    ``null`` (None) coerces to "" too. Without it, two same-priority nodes - one
    with ``created_at: null`` and one with a real timestamp - raise a TypeError
    ("'<' not supported between NoneType and str") mid-sort. _apply_graph_defaults
    never backfills created_at, so null values reach here from real graph.json rows.
    """
    return (PRIORITY_ORDER.get(e.get("priority", "p2"), 2), e.get("created_at") or "")


def _orphan_ids(entries: list[dict]) -> frozenset[str]:
    """Orphan ids for the board, or empty if rollup is unavailable.

    Board rendering runs inside `locked_mutate_graph`, so an exception here
    would break every backlog write. Rollup is a display signal; it fails open
    to the pre-rollup board rather than taking mutations down with it.
    """
    try:
        from fno.graph.rollup import orphan_ids

        return orphan_ids(entries)
    except Exception:  # noqa: BLE001 - display signal; never break a mutation
        return frozenset()


def in_progress_epic_ids(
    entries: list[dict],
    live_claimed: Collection[str] = frozenset(),
) -> frozenset[str]:
    """Parent ids whose work is underway: an epic with a done or claimed child.

    Sessions claim the leaf CHILDREN of an epic, never the container, so an
    in-progress epic carries no claim/session of its own and would otherwise sit
    in its priority column. The board surfaces it in Now by id, derived purely
    from its children - the epic's own ``status`` is never mutated, so the
    ``claimed => session_id`` invariant holds (x-33b2). Mirrors the epics-first
    in-progress signal in ``_intake.make_selection_sort_key``.
    """
    from fno.graph._intake import in_progress_epic_ids as _shared_epic_ids

    return _shared_epic_ids(entries, live_claimed)


def _kanban_column(
    entry: dict,
    in_progress_epics: frozenset[str] = frozenset(),
    live_claimed: frozenset[str] = frozenset(),
    effective_priority: str | None = None,
) -> str | None:
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
    status = entry.get("status", "ready")
    if status in ("deferred", "superseded"):
        return None
    if status == "in_progress":
        return "Now"
    entry_id = entry.get("id")
    # In-progress epic (x-33b2): a container with a done/claimed child has no
    # claim of its own (sessions claim the children) but is genuinely underway,
    # so surface it in Now. The set is derived from children by the caller; the
    # epic's `status` stays honest (never a claim without a session_id).
    if isinstance(entry_id, str) and entry_id in in_progress_epics:
        return "Now"
    # Live-claim overlay (x-4845): a node another session is actively driving
    # holds a LIVE `node:<id>` lockfile but may never write a graph session_id,
    # so its `status` is not "in_progress" and it would otherwise route by bare
    # priority. Surface it in Now off the lockfile truth. Display-only — the
    # graph `status` is never mutated (x-33b2: claimed => session_id stays a
    # pure derivation). Additive: placed below the exclusions so a dead/off-board
    # node is never resurrected, and it only ever promotes to Now.
    if isinstance(entry_id, str) and entry_id in live_claimed:
        return "Now"
    # Queued is orthogonal to status (the field stays set across blocked,
    # idea, etc.). It routes to the Triage lane - a queued node is "awaiting
    # human ack" (via `fno backlog pick`), not active work, so it must NOT
    # inflate Now. Still below claimed: a claimed node is actively in progress
    # and stays in Now even when also queued.
    if entry.get("queued_at"):
        return "Triage"
    priority = effective_priority or entry.get("priority") or "p2"
    if priority in ("p0", "p1"):
        return "Now"
    if priority == "p3":
        return "Later"
    return "Next"


def make_kanban_column(
    entries: list[dict],
    live_claimed: Collection[str] | None = None,
    *,
    strict_claims: bool = False,
):
    """Bind whole-graph overlays into the sole kanban column authority."""
    if live_claimed is None:
        live_claimed = frozenset(live_claimed_node_ids(strict=strict_claims))
    else:
        live_claimed = frozenset(live_claimed)
    in_progress_epics = in_progress_epic_ids(entries, live_claimed)
    priority_for = make_effective_priority(entries)

    def column_for(entry: dict) -> str | None:
        return _kanban_column(
            entry,
            in_progress_epics,
            live_claimed,
            priority_for(entry),
        )

    return column_for


def make_kanban_classifiers(
    entries: list[dict],
    orphans: frozenset[str] | None = None,
    *,
    swimlane: bool = True,
    live_claimed: Collection[str] | None = None,
):
    """Bind one live-claim snapshot into board ordering and column routing."""
    if live_claimed is None:
        live_claimed = frozenset(live_claimed_node_ids())
    else:
        live_claimed = frozenset(live_claimed)
    board_order = make_selection_sort_key(
        entries,
        orphans,
        swimlane=swimlane,
        live_claimed=live_claimed,
    )
    return board_order, make_kanban_column(entries, live_claimed)


def _kanban_card(
    entry: dict, id_to_entry: dict[str, dict], orphans: frozenset[str] = frozenset()
) -> str:
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
    if eid in orphans:
        header += " · [orphan]"
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

    # artifact_url: the user-supplied design/doc link (fno done --link). Shown
    # on the board so the field has a purpose-built reader, not just a writer.
    # Non-http(s) schemes render as code so a markdown auto-linker cannot turn a
    # javascript:/data: URI into a clickable anchor (parity with render_html).
    artifact_url = (entry.get("artifact_url") or "").strip()
    if artifact_url:
        if artifact_url.startswith(("https://", "http://")):
            body_lines.append(f"  artifact: {artifact_url}")
        else:
            body_lines.append(f"  artifact: `{artifact_url}`")

    if body_lines:
        return header + "\n" + "\n".join(body_lines)
    return header


def render_graph_md(
    entries: list[dict], path: Path = GRAPH_MD, *, obsidian: bool = True
) -> None:
    """Render graph.json entries as a Kanban-style markdown board.

    Columns: Now (claimed / p0-p1), Next (p2), Later (p3),
    Triage (queued, awaiting human ack), Done (completed). Within each
    non-Done column, cards use the same selection key as the walker with an
    added project-lane prefix, so per-project clusters are contiguous without
    maintaining a second work-order suffix. Done sorts by completed_at (capped
    at 10).

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
    entries = [e for e in entries if isinstance(e, dict)]
    id_to_entry = {e["id"]: e for e in entries if isinstance(e.get("id"), str)}

    orphans = _orphan_ids(entries)
    board_order, column_for = make_kanban_classifiers(entries, orphans)
    columns: dict[str, list[dict]] = {col: [] for col in KANBAN_COLUMNS}
    for entry in entries:
        col = column_for(entry)
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
            items.sort(key=board_order)

    lines: list[str] = ["---", "kanban-plugin: board", "---", ""] if obsidian else []
    for col in KANBAN_COLUMNS:
        lines.append(f"## {col}")
        lines.append("")
        if not columns[col]:
            lines.append("")
            continue
        for entry in columns[col]:
            lines.append(_kanban_card(entry, id_to_entry, orphans))
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
