"""Graph Kanban rendering as a self-contained HTML file.

Public API:
    render_graph_html(entries, path) -> None

Sibling to render.py's render_graph_md. Same column semantics
(Now/Next/Later/Done) and sort key, different output shape: one HTML
document with a master kanban plus per-project collapsible sections,
inline CSS+JS, no external assets. Done cards are hidden by default
via CSS so the file is useful without JS; a Show done toggle adds the
`show-done` class on <body> to reveal them.
"""
from __future__ import annotations

import datetime as _dt
import html
import os
import tempfile
import urllib.parse
import zlib
from collections import Counter
from pathlib import Path

from fno.graph._constants import GRAPH_HTML
from fno.graph.render import (
    KANBAN_COLUMNS,
    UNSCOPED_LABEL,
    _kanban_column,
    _lane_sort_key,
    _project_key,
    in_progress_epic_ids,
)

# Shared single source of truth with the markdown renderer (render.KANBAN_COLUMNS)
# so the column set + order can never drift between the two boards.
COLUMNS = KANBAN_COLUMNS
# UNSCOPED_LABEL and _project_key are hoisted into render.py (the shared
# ordering engine) and re-exported here so existing importers + tests that
# do `from fno.graph.render_html import UNSCOPED_LABEL` keep working.


def _load_obsidian_vault() -> str | None:
    """Read ``config.obsidian.vault`` from the GLOBAL settings file directly.

    Reads ``~/.fno/settings.yaml`` (or whatever ``FNO_GLOBAL_SETTINGS_PATH``
    resolves to). Deliberately bypasses ``load_settings()`` because that loader
    walks project-local-first-then-global and stops at the first match: when a
    backlog mutation fires from a project whose own ``.fno/settings.yaml``
    lacks an obsidian block, the project-local file shadows the global one and
    the auto-render writes ``~/.fno/graph.html`` (a global artifact) with
    vault=None, zeroing out every Obsidian deep link.

    Vault is a global concept (which vault holds the plan files) and graph.html
    is a global artifact, so the source of truth must be the global file.
    """
    try:
        import yaml
        from fno.config import _global_settings_path
        path = _global_settings_path()
        if not path.is_file():
            return None
        with path.open(encoding="utf-8") as f:
            data = yaml.safe_load(f) or {}
        obs = (data.get("config") or {}).get("obsidian") or {}
        if not obs.get("enabled"):
            return None
        vault = obs.get("vault")
        if isinstance(vault, str) and vault.strip():
            return vault.strip()
        return None
    except Exception:
        return None


# Soft WIP caps applied when ``config.kanban.wip_caps`` is absent entirely.
# Configuring the block at all takes full control (no per-key default merge),
# matching the "if you configure it, you own it" read in _load_wip_caps.
_DEFAULT_WIP_CAPS = {"now": 20, "next": 50}


def _load_wip_caps() -> dict[str, int]:
    """Read ``config.kanban.wip_caps`` from the GLOBAL settings file directly.

    Returns a ``{column_lower: positive_int}`` map. Defensive by construction:
    the HTML auto-render fires inside ``locked_mutate_graph``, so a malformed
    config must degrade to "uncapped" rather than raise (a raise would break
    every backlog mutation). Same global-file rationale as ``_load_obsidian_vault``
    (graph.html is a global artifact).

    - block absent           -> ``_DEFAULT_WIP_CAPS`` (now/next seeded)
    - block present          -> only its entries; invalid ones dropped (uncapped)
    - non-int / <=0 / bool    -> that column is uncapped (omitted), never raised
    """
    try:
        import yaml
        from fno.config import _global_settings_path
        path = _global_settings_path()
        if not path.is_file():
            return dict(_DEFAULT_WIP_CAPS)
        with path.open(encoding="utf-8") as f:
            data = yaml.safe_load(f) or {}
        kanban = (data.get("config") or {}).get("kanban")
        if not isinstance(kanban, dict) or "wip_caps" not in kanban:
            return dict(_DEFAULT_WIP_CAPS)
        raw = kanban.get("wip_caps")
        if not isinstance(raw, dict):
            return {}  # present but malformed -> all columns uncapped
        out: dict[str, int] = {}
        for k, v in raw.items():
            if not isinstance(k, str):
                continue
            # bool subclasses int; a YAML `true` is never a real cap.
            if isinstance(v, bool):
                continue
            if isinstance(v, int) and v > 0:
                out[k.lower()] = v
            # non-int / negative / zero / null -> uncapped (omitted)
        return out
    except Exception:
        return {}


_VAULT_TOPLEVEL_DIRS = ("internal/",)


def _canonicalize_plan_path(plan_path: str | None, vault: str | None = None) -> str | None:
    """Normalize a plan_path to a vault-relative form.

    Tolerates the shapes that have shown up in graph.json:

    - canonical: ``internal/<project>/plans/<slug>(.md|/)``
    - vault-prefixed: ``~/myvault/internal/...`` or
      ``/Users/<user>/myvault/internal/...``. The vault name is stripped
      when a vault is supplied.
    - worktree-rooted: ``~/conductor/workspaces/<repo>/<wt>/internal/...``.
      Falls back to finding the LAST occurrence of ``/internal/``.

    Returns the canonical form (no leading ``/``) or None when the path
    has no recognizable vault-relative segment. Historical ``dev/`` paths
    are deprecated; migrate to ``internal/`` before relying on this.
    """
    if not plan_path:
        return None
    p = plan_path.strip()
    if not p:
        return None
    # Already canonical: starts with a known vault top-level dir.
    if p.startswith(_VAULT_TOPLEVEL_DIRS):
        return p
    # Vault-prefixed path: strip everything up to and including /<vault>/.
    # Only honor the strip when the remainder starts with a canonical
    # top-level dir - otherwise a path like `~/myvault/dev/...` would be
    # accepted under a deprecated `dev/` shape.
    if vault:
        needle = f"/{vault}/"
        idx = p.rfind(needle)
        if idx != -1:
            stripped = p[idx + len(needle):]
            if stripped.startswith(_VAULT_TOPLEVEL_DIRS):
                return stripped
    # Worktree-rooted path: pick the LAST top-level dir occurrence.
    best_idx = -1
    best_marker = ""
    for marker in _VAULT_TOPLEVEL_DIRS:
        idx = p.rfind(f"/{marker}")
        if idx > best_idx:
            best_idx = idx
            best_marker = marker
    if best_idx != -1:
        return p[best_idx + 1:]  # skip the leading slash; keep marker
    return None


def _obsidian_url(vault: str, plan_path: str) -> str | None:
    """Build an ``obsidian://open?vault=...&file=...`` deep link.

    Returns None when the plan_path has no recognizable vault-relative
    segment, or does not point at a markdown file (the ``file`` param
    can only address a file, not a directory).
    """
    canonical = _canonicalize_plan_path(plan_path, vault=vault)
    if canonical is None:
        return None
    p = canonical.rstrip("/")
    if not p.endswith(".md"):
        return None
    target = p[:-3]  # the `file` param wants no extension
    return (
        f"obsidian://open?vault={urllib.parse.quote(vault, safe='')}"
        f"&file={urllib.parse.quote(target, safe='/')}"
    )


def _project_color(name: str | None) -> str:
    """Deterministic project chip color. Unscoped -> neutral gray."""
    if not name:
        return "hsl(0, 0%, 70%)"
    hue = zlib.crc32(name.encode("utf-8")) % 360
    return f"hsl({hue}, 65%, 55%)"


def _column_for(entry: dict, epics: frozenset[str] = frozenset()) -> str | None:
    """Stable column name for an entry; None to exclude (roadmap type)."""
    return _kanban_column(entry, epics)


def _bucket(entries: list[dict]) -> dict[str, list[dict]]:
    """Partition entries into the kanban columns, sorted per column."""
    epics = in_progress_epic_ids(entries)
    cols: dict[str, list[dict]] = {c: [] for c in COLUMNS}
    for e in entries:
        col = _column_for(e, epics)
        if col is None:
            continue
        cols[col].append(e)
    for col, items in cols.items():
        if col == "Done":
            # Intentional divergence from render_graph_md, which caps Done at 10
            # for Obsidian's flat list. Here Done is hidden by default via CSS,
            # so revealing it via the toggle should show the full history.
            items.sort(key=lambda e: e.get("completed_at", ""), reverse=True)
        else:
            # Shared lane key: cluster by project, ranked-before-unranked, then
            # the (priority, created_at) fallback - same order as the md board.
            items.sort(key=_lane_sort_key)
    return cols


def _card_flags(entry: dict) -> list[tuple[str, str]]:
    """Compute the visual flag chips for a card: (css_class, label) pairs.

    Surfaces side-states that no longer claim their own column under the
    intent-based mapping: in-flight sessions, blocked nodes (with a count),
    and ideas that lack a plan.
    """
    flags: list[tuple[str, str]] = []
    status = entry.get("_status") or "ready"
    if status == "claimed":
        flags.append(("flag-claimed", "in session"))
    if entry.get("queued_at") and status not in ("done", "claimed"):
        flags.append(("flag-queued", "queued"))
    if status == "blocked":
        open_blockers = [b for b in (entry.get("blocked_by") or []) if isinstance(b, str)]
        n = len(open_blockers)
        flags.append(("flag-blocked", f"blocked ({n})" if n else "blocked"))
    if status == "idea":
        flags.append(("flag-idea", "needs plan"))
    return flags


def _card_html(entry: dict, id_to_entry: dict[str, dict], vault: str | None = None) -> str:
    eid = html.escape(str(entry.get("id", "?")))
    title = html.escape((entry.get("title") or "").replace("\n", " ").strip() or "(untitled)")
    priority = html.escape(str(entry.get("priority") or "p2"))
    project = _project_key(entry)
    chip_color = _project_color(None if project == UNSCOPED_LABEL else project)
    chip_label = html.escape(project)
    flags = _card_flags(entry)
    # card-level class encodes the side-state for theming the whole card
    # (e.g., left border tint on claimed / blocked / idea entries).
    card_classes = ["card"]
    for flag_class, _ in flags:
        card_classes.append(flag_class)

    parts: list[str] = []
    parts.append(f'<article class="{" ".join(card_classes)}" data-id="{eid}">')
    header_parts = [
        f'<header><span class="prio prio-{priority}">{priority}</span>',
        f'<span class="chip" style="background:{chip_color}">{chip_label}</span>',
    ]
    for flag_class, label in flags:
        header_parts.append(
            f'<span class="flag {flag_class}">{html.escape(label)}</span>'
        )
    header_parts.append(
        f'<button class="eid" type="button" data-copy="{eid}" '
        f'aria-label="Copy {eid} to clipboard">{eid}'
        f'<span class="copy-icon" aria-hidden="true">⎘</span>'
        f'</button></header>'
    )
    parts.append("".join(header_parts))
    parts.append(f'<h3 class="title">{title}</h3>')

    plan_path = entry.get("plan_path")
    if plan_path:
        plan_str = str(plan_path)
        # Render the canonical (vault-relative) form when we can derive it,
        # so worktree-rooted and tilde-prefixed paths display the same way
        # they'd be linked. Falls back to the raw stored value otherwise.
        display_str = _canonicalize_plan_path(plan_str, vault=vault) or plan_str
        obs_url = _obsidian_url(vault, plan_str) if vault else None
        if obs_url:
            parts.append(
                f'<div class="meta plan"><a href="{html.escape(obs_url, quote=True)}">'
                f'{html.escape(display_str)}</a></div>'
            )
        else:
            parts.append(f'<div class="meta plan">{html.escape(display_str)}</div>')

    blockers = [b for b in entry.get("blocked_by", []) if isinstance(b, str)]
    is_done = bool(entry.get("completed_at"))
    if blockers and not is_done:
        open_blockers = []
        for bid in blockers:
            blocker = id_to_entry.get(bid)
            if blocker and not blocker.get("completed_at"):
                btitle = (blocker.get("title") or "?")[:40]
                open_blockers.append(f"{html.escape(bid)} ({html.escape(btitle)})")
        if open_blockers:
            parts.append(f'<div class="meta blockers">blocked by: {", ".join(open_blockers)}</div>')

    if entry.get("deferred_at") and not is_done:
        reason = (entry.get("deferred_reason") or "").strip()
        body = html.escape(reason) if reason else ""
        parts.append(f'<div class="meta deferred">deferred{": " + body if body else ""}</div>')

    pr_url = entry.get("pr_url")
    if pr_url and is_done:
        raw = str(pr_url)
        # Scheme-validate before emitting an anchor; html.escape alone would
        # let `javascript:` URIs through (no <>&"' to encode).
        if raw.startswith(("https://", "http://")):
            href = html.escape(raw, quote=True)
            parts.append(
                f'<div class="meta pr"><a href="{href}" target="_blank" '
                f'rel="noopener">{href}</a></div>'
            )
        else:
            parts.append(f'<div class="meta pr">{html.escape(raw)}</div>')

    if is_done:
        for extra in entry.get("additional_prs") or []:
            if not isinstance(extra, dict):
                continue
            extra_url = extra.get("url")
            extra_num = extra.get("number")
            note_raw = (extra.get("note") or "").strip()
            note_html = f' - {html.escape(note_raw)}' if note_raw else ""
            if extra_url:
                raw_extra = str(extra_url)
                if raw_extra.startswith(("https://", "http://")):
                    href = html.escape(raw_extra, quote=True)
                    parts.append(
                        f'<div class="meta pr"><a href="{href}" target="_blank" '
                        f'rel="noopener">{href}</a>{note_html}</div>'
                    )
                else:
                    # Mirror the primary pr_url fallback: keep the URL visible
                    # as escaped plain text rather than silently dropping it,
                    # so HTML stays consistent with the markdown renderer.
                    parts.append(
                        f'<div class="meta pr">{html.escape(raw_extra)}{note_html}</div>'
                    )
            elif extra_num is not None:
                parts.append(f'<div class="meta pr">#{int(extra_num)}{note_html}</div>')

    parts.append("</article>")
    return "".join(parts)


def _count_html(col: str, count: int, caps: dict[str, int] | None) -> str:
    """Column count chip, with a soft WIP cap when one is configured.

    Capped: ``<count> / <cap>`` with an ``over`` class when count > cap.
    Uncapped (no/invalid cap): the plain count, no ``/ n`` (AC3-EDGE/ERR).
    """
    cap = caps.get(col.lower()) if caps else None
    if isinstance(cap, int) and not isinstance(cap, bool) and cap > 0:
        over = " over" if count > cap else ""
        return f'<span class="count{over}">{count} / {cap}</span>'
    return f'<span class="count">{count}</span>'


def _lane_divider_html(project: str) -> str:
    """A lightweight per-project sub-lane divider for the master board."""
    color = _project_color(None if project == UNSCOPED_LABEL else project)
    return (
        f'<div class="lane">'
        f'<span class="lane-chip" style="background:{color}">{html.escape(project)}</span>'
        f"</div>"
    )


def _board_html(
    columns: dict[str, list[dict]],
    id_to_entry: dict[str, dict],
    vault: str | None = None,
    caps: dict[str, int] | None = None,
    sublanes: bool = False,
) -> str:
    """Render the kanban column grid for a given bucketed entry set.

    Each column is a <details> with its name + count as the <summary>,
    so the user can tap any column header to collapse/expand it. Done +
    Triage ship closed-by-default; Now/Next/Later are open. The JS
    layer persists user-chosen state in localStorage keyed by column
    name so it survives backlog mutations + re-renders.

    ``caps`` adds a soft WIP cap to each column header (master board only;
    per-project sections pass None for a plain count). ``sublanes`` emits a
    per-project divider before each project's run of cards, but only in a
    multi-project column (a single-project column emits none - AC2-EDGE).
    Cards are pre-sorted by the shared lane key, so a divider on each
    project change yields contiguous, labeled runs.
    """
    out: list[str] = ['<div class="cols">']
    for col in COLUMNS:
        col_class = f"col col-{col.lower()}"
        items = columns[col]
        count = len(items)
        # Done + Triage start closed (Triage is the large awaiting-ack pile -
        # see _kanban_column - so it must not flood the open view); Now/Next/
        # Later open. JS overrides with localStorage value when present.
        open_attr = "" if col in ("Done", "Triage") else " open"
        out.append(
            f'<details class="{col_class}" data-col="{col}"{open_attr}>'
            f'<summary><h4>{col} {_count_html(col, count, caps)}</h4></summary>'
        )
        emit_lanes = sublanes and len({_project_key(e) for e in items}) > 1
        last_proj: str | None = None
        for entry in items:
            if emit_lanes:
                proj = _project_key(entry)
                if proj != last_proj:
                    out.append(_lane_divider_html(proj))
                    last_proj = proj
            out.append(_card_html(entry, id_to_entry, vault=vault))
        out.append("</details>")
    out.append("</div>")
    return "".join(out)


def _stats(entries: list[dict]) -> tuple[Counter, Counter]:
    statuses: Counter = Counter()
    projects: Counter = Counter()
    for e in entries:
        if _column_for(e) is None:
            continue
        statuses[e.get("_status") or "ready"] += 1
        projects[_project_key(e)] += 1
    return statuses, projects


_CSS = """\
* { box-sizing: border-box }
html { -webkit-text-size-adjust: 100%; text-size-adjust: 100% }
body { font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
       margin: 0; padding: 0.75rem; background: #fafafa; color: #222 }
/* Done column ships closed-by-default via the <details> open attribute.
   The show-done checkbox bulk-toggles all .col-done details via JS. */

header.page { position: sticky; top: 0; z-index: 10; background: #fafafa;
              display: flex; flex-wrap: wrap; align-items: center; gap: 0.6rem;
              margin: -0.75rem -0.75rem 1rem; padding: 0.6rem 0.75rem;
              border-bottom: 1px solid #ddd }
header.page h1 { font-size: 1rem; margin: 0; flex: 0 0 auto }
.stats { display: flex; flex-wrap: wrap; gap: 0.35rem; font-size: 12px; color: #555 }
.stats span { background: #fff; padding: 0.2rem 0.55rem; border: 1px solid #ddd;
              border-radius: 4px; white-space: nowrap }
.toggle { margin-left: auto; display: flex; align-items: center; gap: 0.5rem;
          cursor: pointer; user-select: none; padding: 0.4rem 0.65rem;
          background: #fff; border: 1px solid #ddd; border-radius: 5px;
          font-size: 13px; min-height: 36px }
.toggle input { margin: 0; width: 18px; height: 18px }

details.board-section, details.project { margin: 0.6rem 0; background: #fff;
                                          border: 1px solid #e2e2e2; border-radius: 6px }
details.board-section > summary, details.project > summary {
    padding: 0.85rem 1rem; cursor: pointer; font-weight: 600;
    font-size: 0.95rem; display: flex; align-items: center; gap: 0.5rem;
    min-height: 44px; user-select: none }
details.board-section > summary .count, details.project > summary .count { font-weight: 400 }
details.board-section[open] > summary, details.project[open] > summary { border-bottom: 1px solid #eee }
details.board-section .board, details.project .board { margin: 0; padding: 0.5rem }

.count { color: #888; font-weight: 400; font-size: 0.78rem; margin-left: 0.2rem }
/* soft WIP-cap overflow: count > cap renders distinct (HTML board only) */
.count.over { color: #d33; font-weight: 700 }

/* per-project sub-lane divider inside a master-board column */
.lane { margin: 0.4rem 0 0.15rem; padding: 0 0.1rem }
.lane-chip { padding: 0.1rem 0.45rem; border-radius: 3px; color: #fff;
             font-weight: 600; font-size: 9px; letter-spacing: 0.04em;
             text-transform: uppercase }

/* mobile-first: stack columns vertically */
.cols { display: grid; grid-template-columns: 1fr; gap: 0.5rem }
details.col { background: #f1f2f4; border-radius: 6px; padding: 0.6rem; min-height: 0 }
details.col > summary { list-style: none; cursor: pointer; user-select: none;
                         padding: 0.2rem 0; min-height: 32px; display: flex;
                         align-items: center }
details.col > summary::-webkit-details-marker { display: none }
details.col > summary h4 { font-size: 0.85rem; margin: 0; color: #555; font-weight: 600;
                            text-transform: uppercase; letter-spacing: 0.04em;
                            display: flex; align-items: center; gap: 0.4rem }
details.col > summary h4::before { content: "▾"; font-size: 1.25em;
                                    color: #555; display: inline-block; width: 1em;
                                    line-height: 1; text-align: center }
details.col:not([open]) > summary h4::before { content: "▸" }
details.col[open] > summary { margin-bottom: 0.5rem }

.card { background: #fff; border-radius: 5px; padding: 0.65rem 0.75rem;
        margin-bottom: 0.5rem; box-shadow: 0 1px 2px rgba(0,0,0,0.06);
        border-left: 3px solid #ddd }
.card header { display: flex; gap: 0.35rem; align-items: center; flex-wrap: wrap; font-size: 11px }
.prio { padding: 0.1rem 0.4rem; border-radius: 3px; font-weight: 600;
        color: #fff; background: #888; font-size: 10px; letter-spacing: 0.05em }
.prio-p0 { background: #d33 } .prio-p1 { background: #e67 }
.prio-p2 { background: #888 } .prio-p3 { background: #aaa }
.chip { padding: 0.1rem 0.45rem; border-radius: 3px; color: #fff; font-weight: 500; font-size: 10px }
.flag { padding: 0.1rem 0.45rem; border-radius: 3px; font-weight: 600;
        font-size: 10px; letter-spacing: 0.02em; text-transform: uppercase }
.flag-claimed { background: #ffe6a8; color: #6a4a00; border: 1px solid #e0b850 }
.flag-queued { background: #d5f3d8; color: #2a5a2a; border: 1px solid #7fc587 }
.flag-blocked { background: #ffd6d6; color: #872020; border: 1px solid #e88a8a }
.flag-idea { background: #e4e8f3; color: #4a5474; border: 1px solid #b7c0d8 }
.card.flag-claimed { border-left-color: #e0b850 }
.card.flag-queued { border-left-color: #7fc587 }
.card.flag-blocked { border-left-color: #e88a8a }
.card.flag-idea { border-left-color: #b7c0d8 }
.eid {
    margin-left: auto;
    color: #888;
    font-family: ui-monospace, monospace;
    font-size: 10px;
    background: #f4f4f6;
    border: 1px solid #e2e2e2;
    padding: 0.2rem 0.4rem;
    border-radius: 4px;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    min-height: 28px;
    user-select: none;
    font: inherit;
    font-family: ui-monospace, monospace;
    font-size: 10px;
    color: #555;
}
.eid:hover, .eid:active { background: #e9e9ec; border-color: #d0d0d4 }
.eid.copied { background: #d8f5d8; border-color: #7bbc7b; color: #2a5d2a }
.eid.copied .copy-icon { display: none }
.eid.copied::after { content: " copied"; font-size: 10px; color: #2a5d2a }
.eid .copy-icon { font-size: 11px; opacity: 0.7 }
.title { font-size: 14px; margin: 0.4rem 0 0.25rem; font-weight: 500; line-height: 1.35;
         word-wrap: break-word; overflow-wrap: anywhere }
.meta { font-size: 12px; color: #666; margin-top: 0.25rem;
        word-wrap: break-word; overflow-wrap: anywhere }
.meta.plan { font-family: ui-monospace, monospace; font-size: 11px }
.meta.plan a { color: #6845c2; text-decoration: none; display: inline-block; padding: 0.2rem 0 }
.meta.plan a:hover { text-decoration: underline }
.meta.blockers { color: #b85 } .meta.deferred { color: #888; font-style: italic }
.meta.pr { padding: 0.2rem 0 }
.meta.pr a { color: #06c; text-decoration: none; word-break: break-all;
             display: inline-block; padding: 0.15rem 0 }
.meta.pr a:hover { text-decoration: underline }

/* hide redundant project chip inside its own project section
   (the section's summary chip already identifies the project) */
details.project .card .chip { display: none }

footer { margin-top: 2rem; padding-top: 0.75rem; border-top: 1px solid #ddd;
         color: #999; font-size: 11px; text-align: center }

/* desktop: side-by-side columns + tighter spacing */
@media (min-width: 768px) {
    body { font-size: 13px; padding: 1rem }
    header.page { margin: -1rem -1rem 1rem; padding: 0.75rem 1rem; gap: 1rem }
    header.page h1 { font-size: 1.1rem }
    .toggle { margin-left: auto; padding: 0.3rem 0.55rem; min-height: 0; font-size: 12px }
    .toggle input { width: 14px; height: 14px }
    .cols { grid-template-columns: repeat(__NCOLS__, 1fr); gap: 0.6rem }
    details.col { min-height: 80px; padding: 0.5rem }
    details.col > summary { min-height: 0 }
    details.col > summary h4 { font-size: 0.78rem; text-transform: none; letter-spacing: 0 }
    details.col > summary h4::before { font-size: 1.15em }
    .card { padding: 0.5rem }
    .title { font-size: 13px }
    .meta { font-size: 11px }
    details.board-section > summary, details.project > summary {
        padding: 0.6rem 0.8rem; font-size: 0.9rem; min-height: 0 }
}
"""

_JS = """\
(function () {
  // Per-column collapse state persisted across re-renders. Keyed by
  // column name so closing Later once stays closed everywhere
  // (master AND per-project sections, since each has its own Later).
  var COL_KEY = 'abi-kanban-col-state';
  function loadColState() {
    try { return JSON.parse(localStorage.getItem(COL_KEY) || '{}'); }
    catch (e) { return {}; }
  }
  function saveColState(state) {
    try { localStorage.setItem(COL_KEY, JSON.stringify(state)); }
    catch (e) { /* private mode, full disk, whatever - swallow */ }
  }
  var colState = loadColState();
  // Apply saved state to all column <details> on load.
  document.querySelectorAll('details.col').forEach(function (el) {
    var name = el.dataset.col;
    if (!name || !(name in colState)) return;
    if (colState[name] === 'closed') el.removeAttribute('open');
    else el.setAttribute('open', '');
  });
  // Persist any subsequent user-driven toggles. The toggle event
  // doesn't bubble, so we listen in capture phase to catch all.
  document.addEventListener('toggle', function (ev) {
    var el = ev.target;
    if (!el || !el.classList || !el.classList.contains('col')) return;
    var name = el.dataset.col;
    if (!name) return;
    var state = loadColState();
    state[name] = el.open ? 'open' : 'closed';
    saveColState(state);
  }, true);

  var toggle = document.getElementById('show-done');
  if (toggle) {
    // Sync checkbox to current Done state on load.
    var doneFirst = document.querySelector('details.col-done');
    toggle.checked = doneFirst ? doneFirst.open : false;
    toggle.addEventListener('change', function () {
      document.querySelectorAll('details.col-done').forEach(function (el) {
        if (toggle.checked) el.setAttribute('open', '');
        else el.removeAttribute('open');
      });
    });
  }
  // Collapse the master section on narrow viewports so the per-project
  // sections are reachable without scrolling past every entry twice.
  var master = document.getElementById('master');
  if (master && window.matchMedia('(max-width: 767px)').matches) {
    master.removeAttribute('open');
  }
  // Tap-to-copy on the .eid badge for cross-device paste workflows.
  // Delegated handler so we don't bind 200+ listeners.
  document.body.addEventListener('click', function (ev) {
    var btn = ev.target.closest && ev.target.closest('.eid[data-copy]');
    if (!btn) return;
    ev.preventDefault();
    var id = btn.getAttribute('data-copy');
    var done = function () {
      btn.classList.add('copied');
      setTimeout(function () { btn.classList.remove('copied'); }, 1400);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(id).then(done).catch(function () {
        // Fall through to legacy path on permission denied.
        legacyCopy(id, done);
      });
    } else {
      legacyCopy(id, done);
    }
  });
  function legacyCopy(text, onSuccess) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.position = 'absolute';
    ta.style.left = '-9999px';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); onSuccess(); }
    catch (e) { /* nothing we can do */ }
    document.body.removeChild(ta);
  }
})();
"""


def render_graph_html(entries: list[dict], path: Path | None = None) -> None:
    """Render graph.json entries as a self-contained HTML kanban file.

    Layout: master kanban (all entries) + per-project collapsible
    sections sorted by node count descending (unscoped last). Done
    cards are hidden by default via CSS so the file is useful with JS
    disabled. Atomic write via tempfile + os.replace, mirroring
    render_graph_md.

    Default path is resolved lazily so tests can monkeypatch
    ``fno.graph._constants.GRAPH_HTML`` without having to also
    patch this module's import-cached reference.
    """
    if path is None:
        from fno.graph._constants import GRAPH_HTML as _CURRENT_GRAPH_HTML
        path = _CURRENT_GRAPH_HTML
    id_to_entry = {e["id"]: e for e in entries if isinstance(e.get("id"), str)}
    statuses, projects = _stats(entries)
    vault = _load_obsidian_vault()
    caps = _load_wip_caps()

    parts: list[str] = []
    parts.append("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">")
    parts.append('<meta name="viewport" content="width=device-width, initial-scale=1">')
    parts.append('<meta name="color-scheme" content="light dark">')
    parts.append("<title>footnote backlog</title>")
    # The desktop grid column count tracks len(COLUMNS) so adding/removing a
    # kanban column never leaves the CSS hardcoded out of sync.
    parts.append(
        f"<style>{_CSS.replace('__NCOLS__', str(len(COLUMNS)))}</style></head><body>"
    )

    parts.append('<header class="page">')
    parts.append("<h1>footnote backlog</h1>")
    parts.append('<div class="stats">')
    total = sum(statuses.values())
    parts.append(f"<span>total {total}</span>")
    for status, n in statuses.most_common():
        parts.append(f"<span>{html.escape(str(status))} {n}</span>")
    parts.append("</div>")
    parts.append('<label class="toggle"><input type="checkbox" id="show-done"> Show done</label>')
    parts.append("</header>")

    master = _bucket(entries)
    master_total = sum(len(items) for items in master.values())
    parts.append(
        f'<details class="board-section" id="master" open>'
        f'<summary>master <span class="count">{master_total}</span></summary>'
    )
    parts.append(
        _board_html(master, id_to_entry, vault=vault, caps=caps, sublanes=True)
    )
    parts.append("</details>")

    project_order = [p for p, _ in projects.most_common() if p != UNSCOPED_LABEL]
    if UNSCOPED_LABEL in projects:
        project_order.append(UNSCOPED_LABEL)

    for project in project_order:
        proj_entries = [e for e in entries if _project_key(e) == project]
        cols = _bucket(proj_entries)
        chip_color = _project_color(None if project == UNSCOPED_LABEL else project)
        summary = (
            f'<summary><span class="chip" style="background:{chip_color}">'
            f'{html.escape(project)}</span> '
            f'<span class="count">{projects[project]}</span></summary>'
        )
        parts.append(f'<details class="project" open>{summary}')
        parts.append(_board_html(cols, id_to_entry, vault=vault))
        parts.append("</details>")

    ts = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    parts.append(f"<footer>rendered {ts}</footer>")
    parts.append(f"<script>{_JS}</script></body></html>")

    content = "".join(parts)
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
