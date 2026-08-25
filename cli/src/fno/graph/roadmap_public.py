"""Public projection selection and Markdown compatibility.

HTML authoring belongs exclusively to :mod:`fno.graph.render_html`.
"""
from __future__ import annotations

from fno.graph.render import (
    _project_key,
    make_kanban_classifiers,
)
from fno.graph.render_html import PUBLIC_BACKLOG_STATUSES, group_for

# Public-facing column set + labels. Active work is folded into Now and the
# internal Triage column is folded into Later; Done is relabeled "Shipped".
_PUBLIC_COLUMNS = (("Now", "Now"), ("Next", "Next"), ("Later", "Later"), ("Done", "Shipped"))


def _public_entries(entries: list[dict], project: str) -> list[dict]:
    return [
        e for e in entries
        if isinstance(e, dict)
        and e.get("public") is not False
        and _project_key(e) == project
    ]


def _columns(entries: list[dict], project: str) -> dict[str, list[dict]]:
    cols: dict[str, list[dict]] = {col: [] for col, _ in _PUBLIC_COLUMNS}
    board_order, column_for = make_kanban_classifiers(entries)
    for e in _public_entries(entries, project):
        col = column_for(e)
        if col == "In Progress":
            col = "Now"
        elif col == "Triage":  # fold the internal triage pile into Later
            col = "Later"
        if col in cols:
            cols[col].append(e)
    for items in cols.values():
        items.sort(key=board_order)
    return cols


def _card_bits(entry: dict) -> tuple[str, str]:
    """Return (title, meta) with only public-safe fields."""
    title = (entry.get("title") or "(untitled)").replace("\n", " ").strip()
    bits = []
    pr = entry.get("priority")
    if pr:
        bits.append(pr)
    size = entry.get("size")
    if size:
        bits.append(str(size))
    return title, " · ".join(bits)


def render_public_roadmap_md(entries: list[dict], project: str) -> str:
    cols = _columns(entries, project)
    out = [f"# {project} roadmap", ""]
    for col, label in _PUBLIC_COLUMNS:
        items = cols[col]
        if not items:
            continue
        out.append(f"## {label}")
        out.append("")
        for e in items:
            title, meta = _card_bits(e)
            out.append(f"- {title}" + (f" _({meta})_" if meta else ""))
        out.append("")
    return "\n".join(out).rstrip() + "\n"


def public_projection_entries(entries: list[dict], project: str) -> list[dict]:
    """The union whose titles must clear the public leak gate."""
    roadmap = [entry for items in _columns(entries, project).values() for entry in items]
    backlog = public_backlog_entries(entries, project)
    by_id: dict[str, dict] = {}
    for entry in [*roadmap, *backlog]:
        key = str(entry.get("id") or id(entry))
        by_id.setdefault(key, entry)
    return list(by_id.values())


def public_backlog_entries(entries: list[dict], project: str) -> list[dict]:
    return [
        entry
        for entry in _public_entries(entries, project)
        if entry.get("status") in PUBLIC_BACKLOG_STATUSES
    ]


def _backlog_sections(entries: list[dict], project: str) -> list[tuple[str, list[dict]]]:
    groups: dict[str, list[dict]] = {}
    status_order = {status: index for index, status in enumerate(PUBLIC_BACKLOG_STATUSES)}
    for entry in public_backlog_entries(entries, project):
        groups.setdefault(group_for(entry), []).append(entry)
    for items in groups.values():
        items.sort(
            key=lambda entry: (
                status_order.get(str(entry.get("status")), 99),
                str(entry.get("priority") or "p2"),
                str(entry.get("title") or "").lower(),
            )
        )
    return [(name, groups[name]) for name in sorted(groups)]


def render_public_roadmap_html(entries: list[dict], project: str) -> str:
    from fno.graph.render_html import render_public_sections_html
    cols = _columns(entries, project)
    sections = [(label, cols[column]) for column, label in _PUBLIC_COLUMNS]
    return render_public_sections_html(
        sections, title=f"{project} roadmap", projection="roadmap"
    )


def render_public_backlog_html(entries: list[dict], project: str) -> str:
    from fno.graph.render_html import render_public_sections_html

    return render_public_sections_html(
        _backlog_sections(entries, project),
        title=f"{project} backlog",
        projection="backlog",
    )
