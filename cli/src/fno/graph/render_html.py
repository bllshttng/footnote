"""Graph Kanban rendering as a self-contained HTML file.

Public API:
    render_graph_html(entries, path) -> None

Sibling to render.py's render_graph_md. Same column semantics
(In Progress/Now/Next/Later/Triage/Done) and sort key, different output shape: one HTML
document with a master kanban plus per-project collapsible sections,
inline CSS+JS, no external assets. Done cards are hidden by default
via CSS so the file is useful without JS; a Show done toggle adds the
`show-done` class on <body> to reveal them.
"""
from __future__ import annotations

import datetime as _dt
import html
import os
import re
import tempfile
import urllib.parse
import zlib
from collections import Counter
from pathlib import Path

from fno.graph.render import (
    KANBAN_COLUMNS,
    UNSCOPED_LABEL,
    _kanban_column,
    _orphan_ids,
    _project_key,
    make_kanban_classifiers,
)

# Shared single source of truth with the markdown renderer (render.KANBAN_COLUMNS)
# so the column set + order can never drift between the two boards.
COLUMNS = KANBAN_COLUMNS
# UNSCOPED_LABEL and _project_key are hoisted into render.py (the shared
# ordering engine) and re-exported here so existing importers + tests that
# do `from fno.graph.render_html import UNSCOPED_LABEL` keep working.

PUBLIC_BACKLOG_STATUSES = ("in_progress", "ready", "blocked", "idea")
GROUPS = (
    ("agents / spawn / dispatch", r"spawn|dispatch|agent|worker|roster|registry|retask|handoff|successor"),
    ("review & attestation", r"review|attest|coverage|verdict|finding|sigma|peer"),
    ("PR / merge / CI", r"\bpr\b|merge|\bci\b|check|smoke|pytest|mypy|lint|guard|workflow"),
    ("identity / session / claims", r"session|identity|claim|short.?id|uuid|lock|liveness|crown|king"),
    ("backlog / graph / board", r"backlog|graph|node|kanban|board|rank|triage|carveout|groom"),
    ("mux / panes / tui", r"\bmux\b|pane|tmux|tui|squad|keymap|menu"),
    ("config / paths / install", r"config|path|install|deploy|doctor|update|version|schema"),
    ("mail & messaging", r"mail|envelope|inbox|message|relay|notify|digest"),
    ("providers / models / routing", r"provider|model|route|harness|codex|claude|gemini|zai|glm|account|quota"),
    ("plans / target / loop", r"plan|target|loop|wave|blueprint|execute|phase|stop.?hook|compact"),
    ("worktree / git", r"worktree|git\b|branch|rebase|checkout"),
    ("observability / cost", r"metric|cost|budget|telemetry|event|observab|watchdog|monitor"),
    ("docs / skills / prose", r"doc\b|docs|skill|readme|prose|style"),
)


def load_render_entries(entries: list[dict] | None = None) -> list[dict]:
    """Overlay archive on a guarded display read or the canonical graph seam."""
    from fno.graph.store import entries_with_archive, read_graph_with_archive

    return read_graph_with_archive() if entries is None else entries_with_archive(entries)


def group_for(entry: dict) -> str:
    haystack = f"{entry.get('title', '')} {entry.get('slug', '')}".lower()
    for name, pattern in GROUPS:
        if re.search(pattern, haystack):
            return name
    return "uncategorized"


def public_title_leaks(entries: list[dict]) -> list[tuple[str, str, tuple[str, ...]]]:
    """Return every public-title offender and every matched leak class."""
    patterns = (
        ("pr-reference", re.compile(r"(?i)(?:\bPR(?:\s*#?\s*|-)\d+\b|#\d+\b)")),
        ("node-id", re.compile(r"\b[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}\b", re.I)),
        ("home-path", re.compile(r"(?:~/(?:[^\s]+)|/(?:Users|home)/[^\s/]+(?:/[^\s]+)?)")),
        (
            "session-id",
            re.compile(
                r"\b(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|ses-[A-Za-z0-9_-]+)\b",
                re.I,
            ),
        ),
    )
    offenders: list[tuple[str, str, tuple[str, ...]]] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        title = str(entry.get("title") or "").replace("\n", " ").strip()
        classes = tuple(name for name, pattern in patterns if pattern.search(title))
        if classes:
            offenders.append((str(entry.get("id") or "?"), title, classes))
    return offenders


def leak_offender_lines(offenders: list[tuple[str, str, tuple[str, ...]]]) -> list[str]:
    """One refusal line per offender, shared by the manual roadmap verb and
    the auto-render so the two leak-gate reports cannot drift apart."""
    return [
        f"  {node_id}: {','.join(classes)}: {title}"
        for node_id, title, classes in offenders
    ]


def atomic_write_documents(documents: dict[Path, str]) -> None:
    """Stage every document before replacing any destination."""
    staged: list[tuple[Path, str]] = []
    try:
        for path, content in documents.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            fd, temp = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(content)
            staged.append((path, temp))
        for path, temp in staged:
            os.replace(temp, path)
    except Exception:
        for _path, temp in staged:
            try:
                os.unlink(temp)
            except OSError:
                pass
        raise


def _load_obsidian_vault() -> str | None:
    """Read ``config.obsidian.vault`` from the GLOBAL config file directly.

    Walks the config.toml-first global candidates via ``read_global_block``
    (``~/.fno/config.toml``, then the legacy ``settings.yaml``). Deliberately
    bypasses ``load_settings()`` because that loader walks project-local-first
    and stops at the first match: when a backlog mutation fires from a project
    whose own ``.fno/settings.yaml`` lacks an obsidian block, the
    project-local file shadows the global one and the auto-render writes
    ``~/.fno/graph.html`` (a global artifact) with vault=None, zeroing out
    every Obsidian deep link.

    Vault is a global concept (which vault holds the plan files) and graph.html
    is a global artifact, so the source of truth must be the global file.
    """
    try:
        # Function-local: keep graph-module load free of config_io's pydantic/yaml.
        from fno.config_io import read_global_block

        obs = read_global_block("obsidian") or {}
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
    """Read ``config.kanban.wip_caps`` from the GLOBAL config file directly
    (config.toml-first candidates, via ``read_global_block``; same walk and
    rationale as ``_load_obsidian_vault``).

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
        # Function-local: keep graph-module load free of config_io's pydantic/yaml.
        from fno.config_io import read_global_block

        kanban = read_global_block("kanban")
        if kanban is None or "wip_caps" not in kanban:
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
    for marker in _VAULT_TOPLEVEL_DIRS:
        idx = p.rfind(f"/{marker}")
        if idx > best_idx:
            best_idx = idx
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


def _column_for(
    entry: dict,
    epics: frozenset[str] = frozenset(),
    live_claimed: frozenset[str] = frozenset(),
    effective_priority: str | None = None,
) -> str | None:
    """Stable column name for an entry; None to exclude (roadmap type)."""
    return _kanban_column(entry, epics, live_claimed, effective_priority)


def _bucket(
    entries: list[dict],
    orphans: frozenset[str] | None = None,
    *,
    ordering_entries: list[dict] | None = None,
    live_claimed: frozenset[str] | None = None,
) -> dict[str, list[dict]]:
    """Partition entries into the kanban columns, sorted per column.

    ``orphans`` MUST be passed when ``entries`` is a subset of the graph (the
    per-project sections). Orphanhood is a whole-graph property - a feature
    whose parent epic sits in another project has no reachable ancestor inside
    a project-scoped slice, so computing it from the subset invents orphans.
    """
    if orphans is None:
        orphans = _orphan_ids(entries)
    ordering_source = ordering_entries if ordering_entries is not None else entries
    board_order, column_for = make_kanban_classifiers(
        ordering_source,
        orphans,
        live_claimed=live_claimed,
    )
    cols: dict[str, list[dict]] = {c: [] for c in COLUMNS}
    for e in entries:
        col = column_for(e)
        if col is None:
            continue
        cols[col].append(e)
    for col, items in cols.items():
        if col == "Done":
            # Intentional divergence from render_graph_md, which caps Done at 10
            # for Obsidian's flat list. Here Done is hidden by default via CSS,
            # so revealing it via the toggle should show the full history.
            items.sort(key=lambda e: e.get("completed_at") or "", reverse=True)
        else:
            items.sort(key=board_order)
    return cols


def _card_flags(
    entry: dict, orphans: frozenset[str] = frozenset()
) -> list[tuple[str, str]]:
    """Compute the visual flag chips for a card: (css_class, label) pairs.

    Surfaces side-states that no longer claim their own column under the
    intent-based mapping: in-flight sessions, blocked nodes (with a count),
    and ideas that lack a plan.
    """
    flags: list[tuple[str, str]] = []
    status = entry.get("status") or "ready"
    if status == "in_progress":
        flags.append(("flag-claimed", "in session"))
    if entry.get("queued_at") and status not in ("done", "in_progress"):
        flags.append(("flag-queued", "queued"))
    if status == "blocked":
        open_blockers = [b for b in (entry.get("blocked_by") or []) if isinstance(b, str)]
        n = len(open_blockers)
        flags.append(("flag-blocked", f"blocked ({n})" if n else "blocked"))
    if status == "idea":
        flags.append(("flag-idea", "needs plan"))
    if entry.get("id") in orphans:
        flags.append(("flag-orphan", "orphan"))
    return flags


def _card_html(
    entry: dict,
    id_to_entry: dict[str, dict],
    vault: str | None = None,
    orphans: frozenset[str] = frozenset(),
) -> str:
    eid = html.escape(str(entry.get("id", "?")))
    title = html.escape((entry.get("title") or "").replace("\n", " ").strip() or "(untitled)")
    priority = html.escape(str(entry.get("priority") or "p2"))
    project = _project_key(entry)
    chip_color = _project_color(None if project == UNSCOPED_LABEL else project)
    chip_label = html.escape(project)
    flags = _card_flags(entry, orphans)
    # card-level class encodes the side-state for theming the whole card
    # (e.g., left border tint on claimed / blocked / idea entries).
    card_classes = ["card"]
    for flag_class, _ in flags:
        card_classes.append(flag_class)

    parts: list[str] = []
    project_attr = html.escape(project, quote=True)
    parts.append(
        f'<article class="{" ".join(card_classes)}" data-id="{eid}" '
        f'data-project="{project_attr}">'
    )
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

    details = " ".join(str(entry.get("details") or "").split())
    if details:
        parts.append(f'<div class="meta details">{html.escape(details)}</div>')

    plan_path = entry.get("plan_path")
    if plan_path:
        plan_str = str(plan_path)
        # Render the canonical (vault-relative) form when we can derive it,
        # so worktree-rooted and tilde-prefixed paths display the same way
        # they'd be linked. Falls back to the raw stored value otherwise.
        display_str = _canonicalize_plan_path(plan_str, vault=vault) or plan_str
        obs_url = _obsidian_url(vault, plan_str) if vault else None
        parts.append('<div class="meta plan planrow">')
        if obs_url:
            parts.append(
                f'<a href="{html.escape(obs_url, quote=True)}">'
                f'{html.escape(display_str)}</a>'
            )
        else:
            parts.append(f'<span>{html.escape(display_str)}</span>')
        parts.append(
            f'<button class="copy-control" type="button" '
            f'data-copy="{html.escape(plan_str, quote=True)}">Copy path</button>'
        )
        if obs_url:
            parts.append(
                f'<button class="copy-control" type="button" '
                f'data-copy="{html.escape(obs_url, quote=True)}">Copy link</button>'
            )
        parts.append("</div>")

    blockers = [b for b in entry.get("blocked_by", []) if isinstance(b, str)]
    is_done = bool(entry.get("completed_at")) or entry.get("status") == "done"
    if blockers and not is_done:
        open_blockers = []
        for bid in blockers:
            blocker = id_to_entry.get(bid)
            if blocker and not blocker.get("completed_at"):
                btitle = (blocker.get("title") or "?")[:40]
                open_blockers.append(f"{html.escape(bid)} ({html.escape(btitle)})")
        if open_blockers:
            parts.append(f'<div class="meta blockers">blocked by: {", ".join(open_blockers)}</div>')

    successors = []
    entry_id = entry.get("id")
    if isinstance(entry_id, str):
        for successor in id_to_entry.values():
            if entry_id not in (successor.get("blocked_by") or []):
                continue
            sid = html.escape(str(successor.get("id") or "?"))
            stitle = html.escape(str(successor.get("title") or "?")[:80])
            successors.append(f"{sid} ({stitle})")
    if successors:
        parts.append(f'<div class="meta successors">unblocks: {", ".join(successors)}</div>')

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

    # artifact_url: the user-supplied design/doc link (done --link). Same
    # scheme-validation as pr_url so a javascript: URI cannot become an anchor.
    artifact_raw = str(entry.get("artifact_url") or "").strip()
    if artifact_raw:
        if artifact_raw.startswith(("https://", "http://")):
            href = html.escape(artifact_raw, quote=True)
            parts.append(
                f'<div class="meta pr artifact"><a href="{href}" target="_blank" '
                f'rel="noopener">{href}</a></div>'
            )
        else:
            parts.append(f'<div class="meta pr artifact">{html.escape(artifact_raw)}</div>')

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
    Triage ship closed-by-default; In Progress/Now/Next/Later are open. The JS
    layer persists user-chosen state in localStorage keyed by column
    name so it survives backlog mutations + re-renders.

    ``caps`` adds a soft WIP cap to each column header (master board only;
    per-project sections pass None for a plain count). ``sublanes`` emits a
    per-project divider before each project's run of cards, but only in a
    multi-project column (a single-project column emits none - AC2-EDGE).
    Cards are pre-sorted by the shared work-order key, so a divider on each
    project change yields contiguous, labeled runs.
    """
    orphans = _orphan_ids(list(id_to_entry.values()))
    out: list[str] = ['<div class="cols">']
    for col in COLUMNS:
        col_class = f"col col-{col.lower().replace(' ', '-')}"
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
            out.append(_card_html(entry, id_to_entry, vault=vault, orphans=orphans))
        out.append("</details>")
    out.append("</div>")
    return "".join(out)


def _projected_card_html(entry: dict, projection: str) -> str:
    """The sole public-card author for roadmap and backlog projections."""
    esc = html.escape
    title = esc(str(entry.get("title") or "(untitled)").replace("\n", " ").strip())
    priority = esc(str(entry.get("priority") or "p2"))
    header_parts = [f'<header><span class="prio prio-{priority}">{priority}</span>']
    size = entry.get("size")
    if size:
        header_parts.append(
            f'<span class="chip" style="background:#888">{esc(str(size))}</span>'
        )
    if projection == "backlog":
        status = esc(str(entry.get("status") or "idea"))
        header_parts.append(f'<span class="flag">{status}</span>')
    header_parts.append("</header>")
    project = html.escape(_project_key(entry), quote=True)
    return (
        f'<article class="card" data-project="{project}">'
        + "".join(header_parts)
        + f'<h3 class="title">{title}</h3></article>'
    )


def _project_order(projects: Counter) -> list[str]:
    """Return deterministic project order with the unscoped bucket last."""
    ordered = [project for project, _ in projects.most_common() if project != UNSCOPED_LABEL]
    if UNSCOPED_LABEL in projects:
        ordered.append(UNSCOPED_LABEL)
    return ordered


def _project_filter_html(projects: list[str]) -> str:
    options = []
    for project in projects:
        options.append(
            '<label class="project-option">'
            f'<input type="checkbox" data-project-option="{html.escape(project, quote=True)}" checked>'
            f'{html.escape(project)}'
            "</label>"
        )
    return (
        '<fieldset class="project-filter" id="project-filter">'
        "<legend>Projects</legend>"
        + "".join(options)
        + "</fieldset>"
    )


def _legacy_render_public_sections_html(
    sections: list[tuple[str, list[dict]]],
    *,
    title: str,
    projection: str,
) -> str:
    """Render public sections with shared document and card primitives."""
    css = _CSS.replace("__NCOLS__", str(max(1, len(sections))))
    total = sum(len(entries) for _label, entries in sections)
    projects = Counter(
        _project_key(entry)
        for _label, section_entries in sections
        for entry in section_entries
    )
    parts = [
        '<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">',
        '<meta name="viewport" content="width=device-width, initial-scale=1">',
        '<meta name="color-scheme" content="light dark">',
        f"<title>{html.escape(title)}</title>",
        f"<style>{css}</style></head><body>",
        f'<header class="page"><h1>{html.escape(title)}</h1>',
        f'<div class="stats"><span>{total} public items</span></div>',
        _project_filter_html(_project_order(projects)),
        "</header>",
        '<div class="cols">',
    ]
    for label, entries in sections:
        slug = re.sub(r"[^a-z0-9]+", "-", label.lower()).strip("-")
        parts.append(
            f'<details class="col col-{slug}" data-col="{html.escape(label, quote=True)}" open>'
            f'<summary><h4>{html.escape(label)} <span class="count">{len(entries)}</span></h4></summary>'
        )
        for entry in entries:
            parts.append(_projected_card_html(entry, projection))
        parts.append("</details>")
    parts.append("</div></body></html>")
    return "".join(parts) + "\n"


def _stats(entries: list[dict]) -> tuple[Counter, Counter]:
    statuses: Counter = Counter()
    projects: Counter = Counter()
    for e in entries:
        if _column_for(e) is None:
            continue
        statuses[e.get("status") or "ready"] += 1
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
.project-filter { display: flex; flex-wrap: wrap; align-items: center; gap: 0.35rem 0.55rem;
                  margin: 0; padding: 0.25rem 0.5rem; border: 1px solid #ddd;
                  border-radius: 5px; background: #fff; font-size: 12px }
.project-filter legend { padding: 0 0.25rem; color: #777; font-size: 11px }
.project-option { display: inline-flex; align-items: center; gap: 0.25rem; cursor: pointer;
                  white-space: nowrap }
.project-option input { margin: 0; width: 15px; height: 15px }
.project-hidden { display: none !important }

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
/* Muted on purpose: an orphan is a question to answer, not an alarm. */
.flag-orphan { background: #f0ece4; color: #6b5f4a; border: 1px solid #d3c7b0 }
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
.planrow { display: flex; flex-wrap: wrap; gap: 0.35rem; align-items: center }
.copy-control { border: 1px solid #d0d0d4; border-radius: 4px; background: #fff;
                color: #555; padding: 0.2rem 0.45rem; cursor: pointer; font: inherit }
.copy-control.copied { background: #d8f5d8; border-color: #7bbc7b; color: #2a5d2a }
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
  var COL_KEY = 'fno-kanban-col-state';
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

  var PROJECT_KEY = 'fno-kanban-project-state';
  function loadProjectState() {
    try {
      var state = JSON.parse(localStorage.getItem(PROJECT_KEY) || '{}');
      return state && typeof state === 'object' && !Array.isArray(state) ? state : {};
    } catch (e) { return {}; }
  }
  function saveProjectState(state) {
    try { localStorage.setItem(PROJECT_KEY, JSON.stringify(state)); }
    catch (e) { /* private mode, full disk, whatever - swallow */ }
  }
  var projectOptions = Array.from(document.querySelectorAll('[data-project-option]'));
  var projectState = loadProjectState();
  var queryProject = new URLSearchParams(window.location.search).get('project');
  var queryMatches = queryProject && projectOptions.some(function (option) {
    return option.dataset.projectOption === queryProject;
  });
  function applyProjectFilter() {
    var selected = new Set();
    projectOptions.forEach(function (option) {
      var project = option.dataset.projectOption;
      if (queryMatches && queryProject) option.checked = project === queryProject;
      else if (Object.prototype.hasOwnProperty.call(projectState, project)) {
        option.checked = projectState[project] !== false;
      }
      if (option.checked) selected.add(project);
    });
    document.querySelectorAll('.card[data-project]').forEach(function (card) {
      card.classList.toggle('project-hidden', !selected.has(card.dataset.project));
    });
  }
  function persistProjectFilter() {
    projectState = {};
    projectOptions.forEach(function (option) {
      projectState[option.dataset.projectOption] = option.checked;
    });
    saveProjectState(projectState);
  }
  projectOptions.forEach(function (option) {
    option.addEventListener('change', function () {
      queryMatches = false;
      applyProjectFilter();
      persistProjectFilter();
    });
  });
  applyProjectFilter();

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
  // Delegated copy handler for ids, plan paths, and Obsidian links.
  document.body.addEventListener('click', function (ev) {
    var btn = ev.target.closest && ev.target.closest('[data-copy]');
    if (!btn) return;
    ev.preventDefault();
    var payload = btn.getAttribute('data-copy');
    var done = function () {
      btn.classList.add('copied');
      setTimeout(function () { btn.classList.remove('copied'); }, 1400);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(payload).then(done).catch(function () {
        // Fall through to legacy path on permission denied.
        legacyCopy(payload, done);
      });
    } else {
      legacyCopy(payload, done);
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


def _legacy_render_graph_html(
    entries: list[dict],
    path: Path | None = None,
    project: str | None = None,
    *,
    all_projects: bool = False,
) -> None:
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
    all_entries = [e for e in entries if isinstance(e, dict)]
    entries = all_entries
    if project and not all_projects:
        entries = [e for e in entries if _project_key(e) == project]
    if path is None:
        from fno.graph._constants import GRAPH_HTML as _CURRENT_GRAPH_HTML
        path = _CURRENT_GRAPH_HTML
    id_to_entry = {e["id"]: e for e in all_entries if isinstance(e.get("id"), str)}
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
    parts.append(_project_filter_html(_project_order(projects)))
    parts.append("</header>")

    # Keep full-graph indexes for relationships, orphanhood, and ordering while
    # emitting only the requested project cards.
    all_orphans = _orphan_ids(all_entries)
    master = _bucket(entries, all_orphans, ordering_entries=all_entries)
    master_total = sum(len(items) for items in master.values())
    parts.append(
        f'<details class="board-section" id="master" open>'
        f'<summary>master <span class="count">{master_total}</span></summary>'
    )
    parts.append(
        _board_html(master, id_to_entry, vault=vault, caps=caps, sublanes=True)
    )
    parts.append("</details>")

    project_order = _project_order(projects)

    # Computed over the FULL graph, then handed to each project slice: the
    # per-project sort and the card flags must agree about who is an orphan.
    for project in project_order:
        proj_entries = [e for e in entries if _project_key(e) == project]
        cols = _bucket(
            proj_entries,
            all_orphans,
            ordering_entries=all_entries,
        )
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


# The dashboard below is the single HTML author for local and public backlog
# surfaces. Its data payload is deliberately separate from its document shape:
# public rows omit private fields, while local rows retain them.
_DASHBOARD_STATUS_ORDER = (
    "in_progress",
    "in_review",
    "ready",
    "blocked",
    "design",
    "idea",
    "deferred",
    "done",
    "superseded",
)

_DASHBOARD_CSS = """\
:root { color-scheme: light dark; --bg:#f7f8fa; --surface:#fff; --ink:#20242b;
        --muted:#707782; --line:#d9dde3; --accent:#3568a8; --done:#3f8b61;
        --blocked:#b06a22; --prog:#7056a8; --idea:#9a7620 }
* { box-sizing:border-box }
body { margin:0; padding:20px; background:var(--bg); color:var(--ink);
       font:14px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif }
header.page { max-width:1180px; margin:0 auto 18px }
h1 { margin:0 0 6px; font-size:24px }
.lede { margin:0 0 16px; color:var(--muted) }
.stats { display:grid; grid-template-columns:repeat(auto-fit,minmax(110px,1fr)); gap:8px; margin-bottom:14px }
.stat { background:var(--surface); border:1px solid var(--line); border-radius:8px; padding:10px 12px }
.stat .n { font-size:22px; font-weight:700 }
.stat .k { color:var(--muted); font-size:11px; text-transform:uppercase; letter-spacing:.05em }
.filters { display:flex; flex-wrap:wrap; gap:8px; align-items:center; background:var(--surface);
           border:1px solid var(--line); border-radius:8px; padding:10px }
input[type=search], input[type=date], select { min-height:34px; border:1px solid var(--line);
  border-radius:6px; padding:5px 8px; background:var(--surface); color:inherit }
input[type=search] { flex:1 1 230px }
.chips { display:flex; flex-wrap:wrap; gap:5px }
.chip { border:1px solid var(--line); border-radius:999px; padding:5px 9px; background:var(--surface);
        color:inherit; cursor:pointer; font:inherit; font-size:12px }
.chip[aria-pressed=true] { background:var(--accent); border-color:var(--accent); color:#fff }
.chip .c { opacity:.7; font-size:11px }
.project-chip { border-radius:5px }
.from { display:flex; align-items:center; gap:5px; color:var(--muted); font-size:12px }
.from input { width:135px }
#shown { color:var(--muted); margin:12px 0 8px }
#board { max-width:1180px; margin:0 auto }
.group { background:var(--surface); border:1px solid var(--line); border-radius:9px; margin:9px 0; overflow:hidden }
.ghead { width:100%; display:flex; align-items:center; gap:9px; padding:10px 12px; border:0;
         background:transparent; color:inherit; text-align:left; cursor:pointer; font:inherit }
.ghead h2 { margin:0; font-size:15px; flex:0 0 auto }
.caret { width:13px; color:var(--muted) }
.tw { display:flex; flex:1; height:7px; overflow:hidden; border-radius:5px; background:var(--line) }
.tw i { display:block; height:100% }
.tw i:nth-child(1), .tw i:nth-child(2) { background:var(--prog) }
.tw i:nth-child(3) { background:var(--done) }
.tw i:nth-child(4) { background:var(--blocked) }
.tw i:nth-child(5), .tw i:nth-child(6) { background:var(--idea) }
.tw i:nth-child(7) { background:#8c929a }
.tw i:nth-child(8) { background:var(--done) }
.tw i:nth-child(9) { background:#a5a9af }
.gc { color:var(--muted); font-size:12px }
.rows { border-top:1px solid var(--line) }
.row { border-bottom:1px solid var(--line) }
.row:last-child { border-bottom:0 }
.row.is-hidden { display:none }
.rmain { width:100%; display:grid; grid-template-columns:auto minmax(0,1fr) auto auto; gap:9px;
         align-items:center; padding:9px 12px; border:0; background:transparent; color:inherit;
         text-align:left; cursor:pointer; font:inherit }
.rid { color:var(--muted); font:12px ui-monospace,SFMono-Regular,monospace }
.rt { overflow-wrap:anywhere }
.meta, .dot { color:var(--muted); font-size:12px; white-space:nowrap }
.pill { display:inline-block; border:1px solid var(--line); border-radius:4px; padding:2px 5px; margin-left:3px; font-size:11px }
.pill.pr-p1 { color:#a33; border-color:#d99 }
.haspl { color:var(--accent); margin-right:5px }
.haspr { color:var(--done); margin-right:5px }
.detail { padding:10px 12px 13px; background:color-mix(in srgb,var(--bg) 70%,var(--surface)); color:inherit }
.kv { display:flex; flex-wrap:wrap; gap:5px 14px; color:var(--muted); font-size:12px; margin-bottom:8px }
.blk { border-left:3px solid var(--blocked); padding-left:9px; margin:7px 0; font-size:12px }
.blk .h { font-weight:700; margin-bottom:3px }
.item { margin:3px 0 }
.planrow { display:flex; flex-wrap:wrap; align-items:center; gap:7px; margin-top:8px }
.plan { overflow-wrap:anywhere; color:var(--muted); font:12px ui-monospace,SFMono-Regular,monospace }
.pbtn { border:1px solid var(--line); border-radius:5px; padding:4px 7px; color:inherit; background:var(--surface); text-decoration:none; cursor:pointer; font:inherit; font-size:12px }
.pbtn.primary { color:var(--accent); border-color:var(--accent) }
.empty { padding:18px; color:var(--muted) }
footer { max-width:1180px; margin:18px auto 0; color:var(--muted); font-size:11px }
@media (max-width:700px) { body { padding:12px } .rmain { grid-template-columns:auto minmax(0,1fr) }
  .meta, .dot { grid-column:2; white-space:normal } }
"""

_DASHBOARD_JS = """\
(function () {
  var DATA = JSON.parse(document.getElementById('data').textContent);
  var NODES = DATA.nodes || [];
  var ORDER = (DATA.status_order || []).slice();
  NODES.forEach(function (node) {
    if (ORDER.indexOf(node.s) < 0) ORDER.push(node.s);
  });
  var LOCAL = document.body.dataset.local === 'true';
  var esc = function (s) { return String(s == null ? '' : s).replace(/[&<>\"]/g, function (c) {
    return {'&':'&amp;','<':'&lt;','>':'&gt;','\"':'&quot;'}[c];
  }); };
  var label = function (s) { return String(s).replace(/_/g, ' ').replace(/\\b\\w/g, function (c) { return c.toUpperCase(); }); };
  var counts = function (nodes) { var result = {}; ORDER.forEach(function (s) { result[s] = 0; });
    nodes.forEach(function (n) { result[n.s] = (result[n.s] || 0) + 1; }); return result; };
  var ALL = counts(NODES);
  var state = { q:'', status:new Set(), projects:new Set(), projectFilterActive:false, group:'', prio:'', size:'', from:'', planOnly:false, prOnly:false, open:{} };
  var PROJECT_KEY = 'fno-kanban-project-state';
  function loadProjects() {
    try {
      var saved = JSON.parse(localStorage.getItem(PROJECT_KEY) || '[]');
      if (Array.isArray(saved)) return { selected:new Set(saved), active:saved.length > 0 };
      if (saved && typeof saved === 'object') {
        if (Array.isArray(saved.selected)) return { selected:new Set(saved.selected), active:saved.active !== false };
        return { selected:new Set(Object.keys(saved).filter(function (p) { return saved[p] === true; })), active:true };
      }
    } catch (e) {}
    return { selected:new Set(), active:false };
  }
  function saveProjects() { try { localStorage.setItem(PROJECT_KEY, JSON.stringify({selected:Array.from(state.projects), active:state.projectFilterActive})); } catch (e) {} }
  var statsEl = document.getElementById('stats');
  var statRows = [['Total', NODES.length, ''], ['In progress', ALL.in_progress || 0, ''],
    ['In review', ALL.in_review || 0, ''], ['Ready', ALL.ready || 0, ''], ['Blocked', ALL.blocked || 0, ''],
    ['Design', ALL.design || 0, ''], ['Idea', ALL.idea || 0, ''], ['Deferred', ALL.deferred || 0, ''],
    ['Done', ALL.done || 0, ''], ['Shipped', NODES.length ? Math.round(100 * (ALL.done || 0) / NODES.length) + '%' : '0%', '']];
  statRows.forEach(function (s) { var d = document.createElement('div'); d.className = 'stat';
    d.innerHTML = '<div class=\"n\">' + s[1] + '</div><div class=\"k\">' + s[0] + '</div>'; statsEl.appendChild(d); });
  document.getElementById('totalCount').textContent = NODES.length;
  document.getElementById('planCount').textContent = NODES.filter(function (n) { return n.pl && n.s !== 'done' && n.s !== 'superseded'; }).length;
  document.getElementById('prCount').textContent = NODES.filter(function (n) { return n.pr; }).length;
  var statusChips = document.getElementById('statusChips');
  ORDER.forEach(function (s) { var b = document.createElement('button'); b.className = 'chip'; b.type = 'button';
    b.dataset.s = s; b.setAttribute('aria-pressed', 'false'); b.innerHTML = esc(label(s)) + ' <span class=\"c\">' + (ALL[s] || 0) + '</span>';
    b.addEventListener('click', function () { if (state.status.has(s)) state.status.delete(s); else state.status.add(s);
      b.setAttribute('aria-pressed', state.status.has(s) ? 'true' : 'false'); render(); }); statusChips.appendChild(b); });
  var projectNames = []; NODES.forEach(function (n) { if (projectNames.indexOf(n.project) < 0) projectNames.push(n.project); });
  projectNames.sort(function (a, b) { return a === 'unscoped' ? 1 : b === 'unscoped' ? -1 : a.localeCompare(b); });
  var projectChips = document.getElementById('projectChips');
  projectNames.forEach(function (p) { var b = document.createElement('button'); b.className = 'chip project-chip'; b.type = 'button';
    b.dataset.project = p; b.textContent = p; b.setAttribute('aria-pressed', 'false'); b.addEventListener('click', function () {
      if (state.projects.has(p)) state.projects.delete(p); else state.projects.add(p); state.projectFilterActive = true; saveProjects(); syncProjects(); render(); }); projectChips.appendChild(b); });
  var loadedProjects = loadProjects(); state.projects = loadedProjects.selected; state.projectFilterActive = loadedProjects.active;
  var queryProject = new URLSearchParams(window.location.search).get('project');
  if (queryProject && projectNames.indexOf(queryProject) >= 0) { state.projects = new Set([queryProject]); state.projectFilterActive = true; }
  function syncProjects() { projectChips.querySelectorAll('[data-project]').forEach(function (b) {
    b.setAttribute('aria-pressed', state.projects.has(b.dataset.project) ? 'true' : 'false'); }); }
  syncProjects();
  var groups = []; NODES.forEach(function (n) { if (groups.indexOf(n.g) < 0) groups.push(n.g); }); groups.sort();
  var groupSel = document.getElementById('groupSel'); groups.forEach(function (g) { var o = document.createElement('option'); o.value = g; o.textContent = g; groupSel.appendChild(o); });
  groupSel.addEventListener('change', function () { state.group = groupSel.value; render(); });
  function fill(sel, key, prefix) { var vals = []; NODES.forEach(function (n) { if (n[key] && vals.indexOf(n[key]) < 0) vals.push(n[key]); }); vals.sort(); vals.forEach(function (v) { var o = document.createElement('option'); o.value = v; o.textContent = prefix + ' ' + v; sel.appendChild(o); }); }
  fill(document.getElementById('prioSel'), 'p', 'Priority'); fill(document.getElementById('sizeSel'), 'sz', 'Size');
  document.getElementById('prioSel').addEventListener('change', function (e) { state.prio = e.target.value; render(); });
  document.getElementById('sizeSel').addEventListener('change', function (e) { state.size = e.target.value; render(); });
  var fromEl = document.getElementById('fromDate'); var stamps = NODES.map(function (n) { return n.u || n.c || ''; }).filter(Boolean).sort();
  if (stamps.length) { fromEl.min = stamps[0]; fromEl.max = stamps[stamps.length - 1]; }
  fromEl.addEventListener('change', function () { state.from = fromEl.value || ''; render(); });
  document.getElementById('q').addEventListener('input', function (e) { state.q = e.target.value.toLowerCase().trim(); render(); });
  function buttonFilter(id, key) { var b = document.getElementById(id); b.addEventListener('click', function () { state[key] = !state[key]; b.setAttribute('aria-pressed', state[key] ? 'true' : 'false'); render(); }); }
  buttonFilter('planOnly', 'planOnly'); buttonFilter('prOnly', 'prOnly');
  var openStatuses = new Set(ORDER.filter(function (s) { return s !== 'done' && s !== 'superseded'; })); openStatuses.forEach(function (s) { state.status.add(s); });
  ORDER.forEach(function (s) { var b = statusChips.querySelector('[data-s="' + s + '"]'); if (b) b.setAttribute('aria-pressed', state.status.has(s) ? 'true' : 'false'); });
  function projectMatch(n) { return !state.projectFilterActive || state.projects.has(n.project); }
  function matches(n) {
    if (!projectMatch(n) || (state.status.size && !state.status.has(n.s))) return false;
    if (state.group && state.group !== n.g) return false; if (state.prio && state.prio !== n.p) return false;
    if (state.size && state.size !== n.sz) return false; if (state.from && (n.s === 'done' || n.s === 'superseded') && (n.u || n.c || '') < state.from) return false;
    if (state.planOnly && !(n.pl && n.s !== 'done' && n.s !== 'superseded')) return false; if (state.prOnly && !n.pr) return false;
    if (state.q && (String(n.id || '') + ' ' + n.t + ' ' + String(n.d || '') + ' ' + String(n.pl || '')).toLowerCase().indexOf(state.q) < 0) return false;
    return true;
  }
  function detail(n) {
    var h = '<div class=\"kv\">';
    if (LOCAL) h += '<span><b>id</b> ' + esc(n.id) + '</span>';
    h += '<span><b>status</b> ' + esc(n.s) + '</span>' + (n.p ? '<span><b>priority</b> ' + esc(n.p) + '</span>' : '') + (n.sz ? '<span><b>size</b> ' + esc(n.sz) + '</span>' : '') + '</div>';
    if (n.bb && n.bb.length) { h += '<div class=\"blk\"><div class=\"h\">Dependencies</div>' + n.bb.map(function (b) { return '<div class=\"item\">blocked by <b>' + esc(b.id) + '</b> ' + esc(b.s) + (b.t ? ' — ' + esc(b.t) : '') + '</div>'; }).join('') + '</div>'; }
    if (n.su && n.su.length) { h += '<div class=\"blk\"><div class=\"h\">Unblocks</div>' + n.su.map(function (b) { return '<div class=\"item\"><b>' + esc(b.id) + '</b> ' + esc(b.s) + (b.t ? ' — ' + esc(b.t) : '') + '</div>'; }).join('') + '</div>'; }
    if (n.sb) h += '<div class=\"blk\"><div class=\"h\">Superseded by</div><div class=\"item\"><b>' + esc(n.sb.id) + '</b> ' + esc(n.sb.s) + (n.sb.t ? ' — ' + esc(n.sb.t) : '') + '</div></div>';
    if (n.d) h += '<p>' + esc(n.d) + '</p>'; else if (LOCAL) h += '<p>No description recorded.</p>';
    if (LOCAL && n.pl) h += '<div class=\"planrow\"><span class=\"plan\">' + esc(n.pl) + '</span>' + (n.link ? '<a class=\"pbtn primary\" href=\"' + esc(n.link) + '\">Open in Obsidian ↗</a>' : '') + '</div>';
    if (LOCAL && n.pr) h += '<div class=\"planrow\">' + (n.pu ? '<a class=\"pbtn primary\" href=\"' + esc(n.pu) + '\">PR #' + esc(n.pr) + '</a>' : '<span class=\"pill\">PR #' + esc(n.pr) + '</span>') + '</div>';
    return h;
  }
  function render() {
    var visible = NODES.filter(matches); document.getElementById('shown').textContent = visible.length + ' of ' + NODES.length + ' nodes shown';
    var board = document.getElementById('board'); board.innerHTML = ''; var by = {}; NODES.forEach(function (n) { (by[n.g] = by[n.g] || []).push(n); });
    Object.keys(by).sort().forEach(function (g) { var rows = by[g], c = counts(rows); var sec = document.createElement('section'); sec.className = 'group';
      var head = document.createElement('button'); head.className = 'ghead'; head.type = 'button'; head.innerHTML = '<span class=\"caret\">▼</span><h2>' + esc(g) + '</h2><span class=\"tw\">' + ORDER.map(function (s) { return c[s] ? '<i style=\"width:' + (100 * c[s] / rows.length) + '%\"></i>' : ''; }).join('') + '</span><span class=\"gc\">' + rows.length + '</span>';
      var list = document.createElement('div'); list.className = 'rows'; head.addEventListener('click', function () { list.hidden = !list.hidden; head.querySelector('.caret').textContent = list.hidden ? '▶' : '▼'; }); sec.appendChild(head);
      rows.forEach(function (n) { var row = document.createElement('div'); row.className = 'row' + (matches(n) ? '' : ' is-hidden'); row.dataset.project = n.project; row.dataset.status = n.s;
        var main = document.createElement('button'); main.className = 'rmain'; main.type = 'button'; main.innerHTML = (LOCAL ? '<span class=\"rid\">' + esc(n.id) + '</span>' : '<span class=\"rid\"></span>') + '<span class=\"rt\">' + esc(n.t) + '</span><span class=\"meta\"><span class=\"pill\">' + esc(n.s) + '</span>' + (n.p ? '<span class=\"pill\">' + esc(n.p) + '</span>' : '') + (n.sz ? '<span class=\"pill\">' + esc(n.sz) + '</span>' : '') + '</span><span class=\"dot\">' + (n.pl ? '<span class=\"haspl\">plan</span>' : '') + (n.pr ? '<span class=\"haspr\">PR</span>' : '') + esc(n.u || n.c || '') + '</span>';
        main.setAttribute('aria-expanded', 'false'); main.addEventListener('click', function () { var old = row.querySelector('.detail'); if (old) { old.remove(); main.setAttribute('aria-expanded', 'false'); return; } var d = document.createElement('div'); d.className = 'detail'; d.innerHTML = detail(n); row.appendChild(d); main.setAttribute('aria-expanded', 'true'); }); row.appendChild(main); list.appendChild(row); }); sec.appendChild(list); board.appendChild(sec); });
  }
  render();
})();
"""


def _dashboard_rows(
    entries: list[dict],
    *,
    local: bool,
    vault: str | None = None,
    context_entries: list[dict] | None = None,
) -> list[dict]:
    """Project graph entries into the canonical dashboard's data contract."""
    source = context_entries if context_entries is not None else entries
    index = {e.get("id"): e for e in source if isinstance(e.get("id"), str)}
    rows: list[dict] = []
    for entry in entries:
        status = "done" if entry.get("completed_at") else str(entry.get("status") or "unknown")
        row: dict[str, object] = {
            "s": status,
            "t": str(entry.get("title") or "(untitled)").replace("\n", " ").strip(),
            "p": str(entry.get("priority") or ""),
            "sz": str(entry.get("size") or ""),
            "g": group_for(entry),
            "project": _project_key(entry),
            "c": str(entry.get("created_at") or "")[:10],
            "u": str(entry.get("touched_at") or "")[:10],
            "pl": bool(entry.get("plan_path")),
            "pr": bool(entry.get("pr_number")),
        }
        if local:
            row.update(
                {
                    "id": str(entry.get("id") or "?"),
                    "d": " ".join(str(entry.get("details") or "").split()),
                    "pl": str(entry.get("plan_path") or ""),
                    "pr": str(entry.get("pr_number") or ""),
                    "pu": (
                        str(entry.get("pr_url") or "")
                        if str(entry.get("pr_url") or "").startswith(
                            ("https://", "http://")
                        )
                        else ""
                    ),
                    "su": [
                        {
                            "id": str(successor.get("id") or "?"),
                            "s": str(successor.get("status") or "unknown"),
                            "t": str(successor.get("title") or "")[:90],
                        }
                        for successor in source
                        if isinstance(successor.get("id"), str)
                        and isinstance(entry.get("id"), str)
                        and entry.get("id") in (successor.get("blocked_by") or [])
                    ],
                    "sb": (
                        {
                            "id": str(entry.get("superseded_by")),
                            "s": str(
                                index.get(entry.get("superseded_by"), {}).get("status")
                                or "not found"
                            ),
                            "t": str(
                                index.get(entry.get("superseded_by"), {}).get("title")
                                or ""
                            )[:90],
                        }
                        if isinstance(entry.get("superseded_by"), str)
                        and entry.get("superseded_by")
                        else None
                    ),
                    "bb": [
                        {
                            "id": bid,
                            "s": str(index.get(bid, {}).get("status") or "not found"),
                            "t": str(index.get(bid, {}).get("title") or "")[:90],
                        }
                        for bid in entry.get("blocked_by") or []
                        if isinstance(bid, str)
                    ],
                }
            )
            plan_path = str(row["pl"])
            if vault and plan_path:
                row["link"] = _obsidian_url(vault, plan_path)
        rows.append(row)
    return rows


def _dashboard_static_detail(row: dict[str, object], *, local: bool) -> str:
    parts = ['<div class="detail">', '<div class="kv">']
    if local:
        parts.append(f'<span><b>id</b> {html.escape(str(row.get("id") or ""))}</span>')
    parts.append(f'<span><b>status</b> {html.escape(str(row.get("s") or ""))}</span>')
    if row.get("p"):
        parts.append(f'<span><b>priority</b> {html.escape(str(row["p"]))}</span>')
    if row.get("sz"):
        parts.append(f'<span><b>size</b> {html.escape(str(row["sz"]))}</span>')
    parts.append("</div>")
    if local:
        for heading, key in (("Dependencies", "bb"), ("Unblocks", "su")):
            related = row.get(key) or []
            if not isinstance(related, list) or not related:
                continue
            parts.append(f'<div class="blk"><div class="h">{heading}</div>')
            for item in related:
                if not isinstance(item, dict):
                    continue
                parts.append(
                    f'<div class="item"><b>{html.escape(str(item.get("id") or "?"))}</b> '
                    f'{html.escape(str(item.get("s") or "unknown"))} '
                    f'{html.escape(str(item.get("t") or ""))}</div>'
                )
            parts.append("</div>")
        if row.get("d"):
            parts.append(f'<p>{html.escape(str(row["d"]))}</p>')
        if row.get("pl"):
            parts.append(
                f'<div class="planrow"><span class="plan">{html.escape(str(row["pl"]))}</span></div>'
            )
    parts.append("</div>")
    return "".join(parts)


def _dashboard_static_html(rows: list[dict], *, local: bool) -> str:
    groups: dict[str, list[dict]] = {}
    for row in rows:
        groups.setdefault(str(row.get("g") or "uncategorized"), []).append(row)
    parts: list[str] = []
    for group in sorted(groups):
        group_rows = groups[group]
        parts.append(
            f'<section class="group"><div class="ghead"><span class="caret">▼</span>'
            f'<h2>{html.escape(group)}</h2><span class="gc">{len(group_rows)}</span></div>'
            '<div class="rows">'
        )
        for row in group_rows:
            project = html.escape(str(row.get("project") or ""), quote=True)
            node_id = html.escape(str(row.get("id") or ""), quote=True) if local else ""
            parts.append(
                f'<div class="row" data-project="{project}" data-status="{html.escape(str(row.get("s") or ""), quote=True)}">'
                f'<div class="rmain"><span class="rid">{node_id}</span>'
                f'<span class="rt">{html.escape(str(row.get("t") or ""))}</span>'
                f'<span class="meta"><span class="pill">{html.escape(str(row.get("s") or ""))}</span></span>'
                f'<span class="dot">{html.escape(str(row.get("u") or row.get("c") or ""))}</span></div>'
                '<details class="fallback-detail"><summary>Details</summary>'
                f'{_dashboard_static_detail(row, local=local)}</details></div>'
            )
        parts.append("</div></section>")
    return "".join(parts)


def _dashboard_html(
    entries: list[dict],
    *,
    title: str,
    local: bool,
    vault: str | None = None,
    context_entries: list[dict] | None = None,
) -> str:
    rows = _dashboard_rows(
        entries, local=local, vault=vault, context_entries=context_entries
    )
    json_module = __import__("json")
    payload = json_module.dumps(
        {"nodes": rows, "status_order": list(_DASHBOARD_STATUS_ORDER)},
        separators=(",", ":"),
    ).replace("&", "\\u0026").replace("<", "\\u003c").replace(">", "\\u003e")
    config = json_module.dumps({"local": local}, separators=(",", ":"))
    generated = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    static_board = _dashboard_static_html(rows, local=local)
    return (
        '<!doctype html><html lang="en"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        f"<title>{html.escape(title)}</title><style>{_DASHBOARD_CSS}</style></head>"
        f'<body data-local="{str(local).lower()}"><header class="page"><h1>{html.escape(title)}</h1>'
        '<p class="lede">Every open node, plus recent closed context. <b id="totalCount">0</b> nodes.</p>'
        '<div class="stats" id="stats"></div><div class="filters">'
        '<input type="search" id="q" placeholder="Search title, id, or description…" aria-label="Search nodes">'
        '<div class="chips" id="statusChips" role="group" aria-label="Filter by status"></div>'
        '<div class="chips" id="projectChips" role="group" aria-label="Filter by project"></div>'
        '<select id="groupSel" aria-label="Filter by group"><option value="">All groups</option></select>'
        '<span class="from">from <input type="date" id="fromDate" aria-label="From date"></span>'
        '<select id="prioSel" aria-label="Filter by priority"><option value="">All priorities</option></select>'
        '<select id="sizeSel" aria-label="Filter by size"><option value="">All sizes</option></select>'
        '<button class="chip" id="planOnly" type="button" aria-pressed="false">Plan, unfinished <span class="c" id="planCount"></span></button>'
        '<button class="chip" id="prOnly" type="button" aria-pressed="false">has a PR <span class="c" id="prCount"></span></button>'
        f'</div><div id="shown"></div></header><main id="board">{static_board}</main>'
        f'<footer>rendered {generated}</footer><script id="config" type="application/json">{config}</script>'
        f'<script id="data" type="application/json">{payload}</script><script>{_DASHBOARD_JS}</script></body></html>\n'
    )


def render_graph_html(
    entries: list[dict],
    path: Path | None = None,
    project: str | None = None,
    *,
    all_projects: bool = False,
) -> None:
    """Render the canonical full-detail dashboard for the local surface."""
    all_entries = [entry for entry in entries if isinstance(entry, dict)]
    scoped = (
        all_entries
        if all_projects or not project
        else [entry for entry in all_entries if _project_key(entry) == project]
    )
    if path is None:
        from fno.graph._constants import GRAPH_HTML

        path = GRAPH_HTML
    vault = _load_obsidian_vault()
    content = _dashboard_html(
        scoped,
        title="fno Backlog",
        local=True,
        vault=vault,
        context_entries=all_entries,
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(content)
        os.replace(temp, str(path))
    except Exception:
        try:
            os.unlink(temp)
        except OSError:
            pass
        raise


def render_public_sections_html(
    sections: list[tuple[str, list[dict]]], *, title: str, projection: str
) -> str:
    """Render any public projection with the same canonical dashboard shape."""
    entries = [entry for _label, section in sections for entry in section]
    return _dashboard_html(entries, title=title, local=False)
