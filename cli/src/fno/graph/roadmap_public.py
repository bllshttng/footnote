"""Public roadmap renderer: a curated, leak-free view of the backlog.

Filters to nodes explicitly flagged ``public: true`` (set via
``fno backlog update --public``) for one project, and emits ONLY
titles + priority + size grouped into Now / Next / Later / Shipped.
No node IDs, no plan paths, no cwd - nothing internal leaks. Safe to
commit to a public OSS repo or host on a marketing site.

Reuses the existing column mapping + lane ordering from ``render`` so
the public roadmap can never drift from the real board.
"""
from __future__ import annotations

import html as _html

from fno.graph.render import (
    UNSCOPED_LABEL,
    _kanban_column,
    _lane_sort_key,
    _project_key,
)

# Public-facing column set + labels. The internal Triage column (awaiting
# human ack) is folded into Later for the public view; Done is relabeled
# "Shipped".
_PUBLIC_COLUMNS = (("Now", "Now"), ("Next", "Next"), ("Later", "Later"), ("Done", "Shipped"))


def _public_entries(entries: list[dict], project: str) -> list[dict]:
    return [
        e for e in entries
        if e.get("public") is True and _project_key(e) == project
    ]


def _columns(entries: list[dict], project: str) -> dict[str, list[dict]]:
    cols: dict[str, list[dict]] = {col: [] for col, _ in _PUBLIC_COLUMNS}
    for e in _public_entries(entries, project):
        col = _kanban_column(e)
        if col == "Triage":  # fold the internal triage pile into Later
            col = "Later"
        if col in cols:
            cols[col].append(e)
    for items in cols.values():
        items.sort(key=_lane_sort_key)
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


def _public_card_html(entry: dict) -> str:
    """The real board's card, minus every internal/leaky field.

    Keeps the visual chrome (priority chip, project chip, status flag,
    title) but drops the eid copy-button, plan path, blocker IDs,
    deferred reason, and PR URLs - so it looks like the live board
    without exposing anything private.
    """
    from fno.graph.render_html import _card_flags, _project_color

    esc = _html.escape
    title = esc((entry.get("title") or "").replace("\n", " ").strip() or "(untitled)")
    priority = esc(str(entry.get("priority") or "p2"))
    project = _project_key(entry)
    chip_color = _project_color(None if project == UNSCOPED_LABEL else project)
    flags = _card_flags(entry)

    classes = ["card"] + [fc for fc, _ in flags]
    head = [
        f'<header><span class="prio prio-{priority}">{priority}</span>',
        f'<span class="chip" style="background:{chip_color}">{esc(project)}</span>',
    ]
    for fc, label in flags:
        head.append(f'<span class="flag {fc}">{esc(label)}</span>')
    head.append("</header>")
    return (
        f'<article class="{" ".join(classes)}">'
        + "".join(head)
        + f'<h3 class="title">{title}</h3></article>'
    )


def render_public_roadmap_html(entries: list[dict], project: str) -> str:
    """Render the public roadmap with the live board's exact look.

    Reuses ``render_html._CSS`` so cards, columns, and colors match the
    real ``graph.html`` board, but emits only ``public``-flagged nodes
    and strips every internal field. Native ``<details>`` columns mean
    no JS is needed.
    """
    from fno.graph.render_html import _CSS

    cols = _columns(entries, project)
    esc = _html.escape
    css = _CSS.replace("__NCOLS__", str(len(_PUBLIC_COLUMNS)))
    total = sum(len(v) for v in cols.values())

    parts = [
        '<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">',
        '<meta name="viewport" content="width=device-width, initial-scale=1">',
        '<meta name="color-scheme" content="light dark">',
        f"<title>{esc(project)} roadmap</title>",
        f"<style>{css}</style></head><body>",
        f'<header class="page"><h1>{esc(project)} roadmap</h1>',
        f'<div class="stats"><span>{total} public items</span></div></header>',
        '<div class="cols">',
    ]
    for col, label in _PUBLIC_COLUMNS:
        items = cols[col]
        open_attr = "" if col == "Done" else " open"
        parts.append(
            f'<details class="col col-{col.lower()}" data-col="{col}"{open_attr}>'
            f'<summary><h4>{esc(label)} <span class="count">{len(items)}</span></h4></summary>'
        )
        for e in items:
            parts.append(_public_card_html(e))
        parts.append("</details>")
    parts.append("</div></body></html>")
    return "".join(parts) + "\n"
