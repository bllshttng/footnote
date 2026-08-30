"""The canonical backlog dashboard, rendered as a self-contained HTML file.

Public API:
    render_graph_html(entries, path) -> None      # local, full detail
    render_public_sections_html(sections, ...)    # public, title-gated

One renderer authors every backlog surface: the local files, the
token-gated mux view, and the public projection. They share the whole
skeleton (stat cards, status chips, project chips, search, from-date,
priority and size selects, collapsible groups, row detail) and differ
only in the fields the payload carries. Inline CSS+JS, no external
assets; a static board renders the same rows with JS disabled.
"""
from __future__ import annotations

import datetime as _dt
import html
import json
import os
import re
import tempfile
import urllib.parse
from pathlib import Path

from fno.graph.render import (
    KANBAN_COLUMNS,
    UNSCOPED_LABEL,  # noqa: F401  re-exported for importers; see the note below
    _project_key,
)
# The status rule lives in statuses.py beside its closure test: one copy per
# renderer is how the dashboard and the public roadmap came to disagree.
from fno.graph.statuses import derived_status as _row_status

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


# The one leak vocabulary. The gate below scans titles with it; a test scans a
# whole rendered public document with the same list, so the probe and the gate
# can never drift into disagreeing about what counts as a leak.
LEAK_PATTERNS: tuple[tuple[str, "re.Pattern[str]"], ...] = (
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


def public_title_leaks(entries: list[dict]) -> list[tuple[str, str, tuple[str, ...]]]:
    """Return every public-title offender and every matched leak class."""
    offenders: list[tuple[str, str, tuple[str, ...]]] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        title = str(entry.get("title") or "").replace("\n", " ").strip()
        classes = tuple(name for name, pattern in LEAK_PATTERNS if pattern.search(title))
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

# Closed work. The scripted board leaves these chips unpressed on first paint
# (a roadmap presses done, since shipped work is its point), and the static
# half honors the same rule so the two agree before any script runs.
_DASHBOARD_TERMINAL_STATUSES = ("done", "superseded")

# Status colours live here, keyed by NAME. They used to be positional
# .tw i:nth-child(N) rules, but render() emits no segment for a zero-count
# status, so every surviving segment slid into the wrong colour slot and the
# colours shifted live as the user filtered. A status past the ninth got no
# rule at all.
# One token per status, the SAME token its pill uses, so a bar segment and the
# pill beside it can never disagree about what colour a status is. Every name
# here is defined in _DASHBOARD_CSS for both themes.
_DASHBOARD_STATUS_COLORS = {
    "in_progress": "var(--prog)",
    "in_review": "var(--prog)",
    "ready": "var(--ready)",
    "blocked": "var(--blocked)",
    "design": "var(--idea)",
    "idea": "var(--idea)",
    "deferred": "var(--defer)",
    "done": "var(--done)",
    "superseded": "var(--sup)",
}
# Types that earn a badge. `feature` is 87 percent of the graph (4216 of 4700
# measured 2026-08-27), so badging it is noise; it renders none. epic and bug
# get their own colour, everything else a muted one.
_DASHBOARD_UNBADGED_TYPE = "feature"
_DASHBOARD_TYPE_CLASSES = {"epic": "t-epic", "bug": "t-bug"}
_DASHBOARD_TYPE_FALLBACK = "t-other"

_DASHBOARD_UNKNOWN_COLOR = "var(--muted)"


def _dashboard_status_class(status: object) -> str:
    """The `s-<status>` modifier, or "" for a status the stylesheet does not know.

    An unknown status must fall through to the neutral `.pill` base rather than
    emit a class with no rule, which renders as unstyled text.
    """
    name = str(status or "")
    return f"s-{name}" if name in _DASHBOARD_STATUS_ORDER else ""

_DASHBOARD_CSS = """\
:root {
  --bg:#f6f5f9; --surface:#ffffff; --surface-2:#efedf4; --line:#dedae8;
  --ink:#1a1922; --ink-2:#4a4757; --muted:#6b677e;
  --accent:#9a5418; --accent-soft:#f2e4d4;
  --ready:#9a5f08; --ready-bg:#fdf0dc;
  --done:#1f7358; --done-bg:#dff0e8;
  --idea:#5c5975; --idea-bg:#e9e7f0;
  --sup:#7c5c8c; --sup-bg:#efe6f3;
  --prog:#3b5bbf; --prog-bg:#e2e7f8;
  --defer:#62606e; --defer-bg:#e6e4ec;
  --blocked:#a8331f; --blocked-bg:#f8e2de;
  --bug:#a8331f; --bug-bg:#f8e2de;
  --epic:#7c5c8c; --epic-bg:#efe6f3;
  --p1:#a8331f;
  --shadow:0 1px 2px rgba(26,25,34,.06),0 4px 14px rgba(26,25,34,.05);
}
@media (prefers-color-scheme:dark) {
  :root:not([data-theme="light"]) {
    --bg:#121118; --surface:#1a1922; --surface-2:#22202b; --line:#302d3d;
    --ink:#eae8f2; --ink-2:#bdb9cd; --muted:#8e8aa3;
    --accent:#e09a55; --accent-soft:#33261a;
    --ready:#f0ad4e; --ready-bg:#3a2b13;
    --done:#59c79c; --done-bg:#14342a;
    --idea:#9d99b5; --idea-bg:#26242f;
    --sup:#b490c4; --sup-bg:#2d2337;
    --prog:#7f9cf5; --prog-bg:#1d2440;
    --defer:#8b8799; --defer-bg:#232130;
    --blocked:#f0715c; --blocked-bg:#3a1d18;
    --bug:#f0715c; --bug-bg:#3a1d18;
    --epic:#b490c4; --epic-bg:#2d2337;
    --p1:#f0715c;
    --shadow:0 1px 2px rgba(0,0,0,.4),0 4px 16px rgba(0,0,0,.3);
  }
}
:root[data-theme="dark"] {
  --bg:#121118; --surface:#1a1922; --surface-2:#22202b; --line:#302d3d;
  --ink:#eae8f2; --ink-2:#bdb9cd; --muted:#8e8aa3;
  --accent:#e09a55; --accent-soft:#33261a;
  --ready:#f0ad4e; --ready-bg:#3a2b13;
  --done:#59c79c; --done-bg:#14342a;
  --idea:#9d99b5; --idea-bg:#26242f;
  --sup:#b490c4; --sup-bg:#2d2337;
  --prog:#7f9cf5; --prog-bg:#1d2440;
  --defer:#8b8799; --defer-bg:#232130;
  --blocked:#f0715c; --blocked-bg:#3a1d18;
  --bug:#f0715c; --bug-bg:#3a1d18;
  --epic:#b490c4; --epic-bg:#2d2337;
  --p1:#f0715c;
  --shadow:0 1px 2px rgba(0,0,0,.4),0 4px 16px rgba(0,0,0,.3);
}
* { box-sizing:border-box }
body { margin:0; background:var(--bg); color:var(--ink);
  font-family:"IBM Plex Sans",ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;
  font-size:15px; line-height:1.5; -webkit-font-smoothing:antialiased }
.wrap { max-width:1240px; margin:0 auto; padding:32px 24px 96px; display:flex; flex-direction:column; gap:24px }
header { display:flex; flex-direction:column; gap:10px }
.eyebrow { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:11px; letter-spacing:.13em;
  text-transform:uppercase; color:var(--muted) }
h1 { font-size:29px; line-height:1.15; margin:0; font-weight:600; letter-spacing:-.02em; text-wrap:balance }
.lede { margin:0; color:var(--ink-2); max-width:66ch }
.lede b { color:var(--ink); font-weight:600 }
.stats { display:grid; grid-template-columns:repeat(auto-fit,minmax(132px,1fr)); gap:10px }
.stat { background:var(--surface); border:1px solid var(--line); border-radius:9px;
  padding:13px 15px; display:flex; flex-direction:column; gap:3px; box-shadow:var(--shadow) }
.stat .n { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:26px; font-weight:600;
  font-variant-numeric:tabular-nums; line-height:1.1; letter-spacing:-.02em }
.stat .k { font-size:11px; letter-spacing:.09em; text-transform:uppercase; color:var(--muted); font-weight:500 }
.stat.is-ready { border-color:var(--ready); background:var(--ready-bg) }
.stat.is-ready .n, .stat.is-ready .k { color:var(--ready) }
.stat.is-done .n { color:var(--done) }
.stat.is-prog { border-color:var(--prog); background:var(--prog-bg) }
.stat.is-prog .n, .stat.is-prog .k { color:var(--prog) }
.stat.is-blocked .n { color:var(--blocked) }
.controls { display:flex; flex-wrap:wrap; gap:9px; align-items:center;
  background:var(--surface); border:1px solid var(--line); border-radius:9px;
  padding:11px 13px; box-shadow:var(--shadow); position:sticky; top:0; z-index:20 }
input[type=search] { flex:1 1 240px; min-width:180px; background:var(--surface-2); color:var(--ink);
  border:1px solid var(--line); border-radius:7px; padding:8px 11px; font-family:inherit; font-size:14px }
input[type=search]::placeholder { color:var(--muted) }
input[type=search]:focus-visible, button:focus-visible, select:focus-visible {
  outline:2px solid var(--accent); outline-offset:2px }
select { background:var(--surface-2); color:var(--ink); border:1px solid var(--line);
  border-radius:7px; padding:8px 10px; font-family:inherit; font-size:14px; max-width:100% }
.datef { display:inline-flex; align-items:center; gap:7px; font-size:12.5px; color:var(--muted);
  background:var(--surface-2); border:1px solid var(--line); border-radius:7px; padding:5px 10px;
  font-family:"IBM Plex Mono",ui-monospace,monospace }
.datef input { background:transparent; border:0; color:var(--ink); font-family:inherit;
  font-size:12.5px; padding:2px 0; min-width:118px }
.datef input:focus-visible { outline:2px solid var(--accent); outline-offset:3px }
.datef.on { border-color:var(--accent); color:var(--accent) }
.chips { display:flex; gap:6px; flex-wrap:wrap }
.chip { border:1px solid var(--line); background:var(--surface-2); color:var(--ink-2);
  border-radius:999px; padding:6px 12px; font-size:12.5px; font-weight:500; cursor:pointer;
  font-family:inherit; display:inline-flex; align-items:center; gap:6px }
.chip .c { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:11px; color:var(--muted);
  font-variant-numeric:tabular-nums }
.chip[aria-pressed=true] { background:var(--accent-soft); border-color:var(--accent);
  color:var(--accent); font-weight:600 }
.chip[aria-pressed=true] .c { color:var(--accent) }
.project-chip { border-radius:6px }
.group { background:var(--surface); border:1px solid var(--line); border-radius:9px;
  overflow:hidden; box-shadow:var(--shadow) }
.ghead { display:flex; align-items:center; gap:12px; padding:12px 15px; cursor:pointer;
  background:var(--surface-2); border:0; width:100%; text-align:left; font-family:inherit; color:var(--ink) }
.ghead h2 { font-size:15px; margin:0; font-weight:600; flex:1; letter-spacing:-.01em }
.ghead .tw { display:flex; height:7px; width:150px; border-radius:99px; overflow:hidden;
  background:var(--surface); flex:0 0 auto }
.ghead .tw i { display:block; height:100% }
.ghead .gc { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:12px; color:var(--muted);
  font-variant-numeric:tabular-nums; flex:0 0 auto }
.ghead .caret { color:var(--muted); font-size:11px; transition:transform .15s ease; flex:0 0 auto }
.group[data-open=false] .caret { transform:rotate(-90deg) }
.group[data-open=false] .rows { display:none }
.rows { display:flex; flex-direction:column }
.row { border-top:1px solid var(--line) }
.row.is-hidden, .group.is-hidden { display:none }
.row:target { background:var(--accent-soft); scroll-margin-top:96px }
.rmain { display:grid; grid-template-columns:78px 1fr auto auto; gap:11px; align-items:baseline;
  padding:9px 15px; cursor:pointer; width:100%; background:none; border:0;
  font-family:inherit; color:var(--ink); text-align:left; font-size:14px }
.rmain:hover { background:var(--surface-2) }
body[data-local="true"] .rid { cursor:copy; border-radius:4px }
body[data-local="true"] .rid:hover { background:var(--accent-soft); color:var(--accent) }
.rid.ok, .pbtn.ok { background:var(--done-bg); color:var(--done); border-color:var(--done) }
/* A public board emits an empty .rid, so the fixed id column would be a
   permanent empty gutter on every row. Remove the TRACK, not just its width:
   a zero-width column still takes the 11px grid gap, leaving a smaller gutter
   rather than none. */
body[data-local="false"] .rid { display:none }
body[data-local="false"] .rmain { grid-template-columns:1fr auto auto }
/* 104px = 15px padding + the 78px id column + its 11px gap. Collapse the
   column and the indent it was aligned to has to go with it. */
body[data-local="false"] .detail { padding-left:15px }
.rid { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:12.5px; color:var(--muted) }
.rt { line-height:1.4; min-width:0; overflow-wrap:anywhere }
.row[data-s=superseded] .rt { color:var(--muted); text-decoration:line-through;
  text-decoration-color:var(--sup); text-decoration-thickness:1px }
.row[data-s=done] .rt { color:var(--ink-2) }
.meta { display:flex; gap:5px; align-items:center; flex:0 0 auto }
/* The neutral chip IS the base. A priority, a size, a PR number and any status
   outside ORDER all render as a bare `pill` with no modifier, so a base that
   carries only typography leaves them as loose uppercase text. */
.pill { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:10.5px; font-weight:500;
  padding:2px 7px; border-radius:5px; letter-spacing:.04em; text-transform:uppercase; white-space:nowrap;
  background:var(--surface-2); color:var(--ink-2) }
.s-ready { background:var(--ready-bg); color:var(--ready); font-weight:600 }
.s-done { background:var(--done-bg); color:var(--done) }
.s-idea { background:var(--idea-bg); color:var(--idea) }
.s-superseded { background:var(--sup-bg); color:var(--sup) }
.s-in_progress { background:var(--prog-bg); color:var(--prog); font-weight:600 }
.s-blocked { background:var(--blocked-bg); color:var(--blocked); font-weight:600 }
.s-deferred { background:var(--defer-bg); color:var(--defer) }
.s-in_review { background:var(--prog-bg); color:var(--prog); font-weight:600 }
.s-design { background:var(--idea-bg); color:var(--idea) }
.t-epic { background:var(--epic-bg); color:var(--epic); font-weight:600 }
.t-bug { background:var(--bug-bg); color:var(--bug); font-weight:600 }
.t-other { background:var(--surface-2); color:var(--muted) }
.pr-p1 { color:var(--p1); font-weight:700 }
.dot { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:11px; color:var(--muted);
  white-space:nowrap; font-variant-numeric:tabular-nums }
.votes { color:var(--accent); cursor:copy; border-radius:4px; padding:1px 4px }
.votes:hover { background:var(--accent-soft) }
.haspl { color:var(--accent) }
.haspr { color:var(--done) }
.kids { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:11px; color:var(--muted);
  display:inline-flex; align-items:center; gap:5px }
.kids .tw { display:flex; height:6px; width:52px; border-radius:99px; overflow:hidden;
  background:var(--surface-2) }
.kids .tw i { display:block; height:100% }
.detail { padding:2px 15px 16px 104px; display:flex; flex-direction:column; gap:9px;
  background:var(--surface-2); border-top:1px dashed var(--line) }
.detail p { margin:0; color:var(--ink-2); font-size:13.5px; line-height:1.62; max-width:88ch }
.detail .kv { display:flex; flex-wrap:wrap; gap:6px 18px; font-size:12px; color:var(--muted) }
.detail .kv span { font-family:"IBM Plex Mono",ui-monospace,monospace }
.detail .kv b { color:var(--ink-2); font-weight:500 }
/* The kv line is 12px text, so the shared .pbtn padding would tower over it. */
.detail .kv .pbtn { padding:1px 6px; font-size:11px }
.blk { border:1px solid var(--blocked); background:var(--blocked-bg); border-radius:7px;
  padding:9px 11px; display:flex; flex-direction:column; gap:6px }
.blk.kin { border-color:var(--line); background:var(--surface) }
.blk.kin .h { color:var(--ink-2) }
.blk .h { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:10.5px; letter-spacing:.09em;
  text-transform:uppercase; color:var(--blocked); font-weight:600 }
.blk .item { font-size:13px; color:var(--ink-2); line-height:1.5 }
.blk .nid { font-family:"IBM Plex Mono",ui-monospace,monospace; color:var(--ink); font-weight:600 }
.blk a.nid { text-decoration:none; border-bottom:1px dotted var(--accent); color:var(--accent) }
.blk a.nid:hover { border-bottom-style:solid }
.blk .st { font-family:"IBM Plex Mono",ui-monospace,monospace; font-size:10.5px; padding:1px 6px;
  border-radius:4px; background:var(--surface); border:1px solid var(--line); color:var(--muted) }
.blk .stale { color:var(--blocked); border-color:var(--blocked); font-weight:600 }
.planrow { display:flex; flex-wrap:wrap; gap:6px; align-items:stretch }
.plan { flex:1 1 300px; min-width:0; font-family:"IBM Plex Mono",ui-monospace,monospace;
  font-size:11.5px; color:var(--ink-2); word-break:break-all; background:var(--surface);
  border:1px solid var(--line); border-radius:6px; padding:6px 9px; line-height:1.45 }
.pbtn { border:1px solid var(--line); background:var(--surface); color:var(--ink-2);
  border-radius:6px; padding:5px 10px; font-size:11.5px; font-weight:500;
  font-family:"IBM Plex Mono",ui-monospace,monospace; cursor:pointer; white-space:nowrap;
  display:inline-flex; align-items:center; gap:5px; text-decoration:none }
.pbtn:hover { border-color:var(--accent); color:var(--accent) }
.pbtn.primary { border-color:var(--accent); color:var(--accent); background:var(--accent-soft) }
.pbtn:focus-visible { outline:2px solid var(--accent); outline-offset:2px }
.none, .empty { padding:26px 15px; color:var(--muted); text-align:center; font-size:14px }
footer { color:var(--muted); font-size:12px; border-top:1px solid var(--line); padding-top:14px;
  display:flex; flex-wrap:wrap; gap:5px 16px }
footer span { font-family:"IBM Plex Mono",ui-monospace,monospace }
@media (max-width:720px) {
  .rmain { grid-template-columns:66px 1fr; row-gap:5px }
  .meta, .dot { grid-column:2; white-space:normal }
  body[data-local="false"] .rmain { grid-template-columns:1fr }
  body[data-local="false"] .meta, body[data-local="false"] .dot { grid-column:1 }
  .detail { padding-left:15px }
  .ghead .tw { display:none }
  .wrap { padding:16px 12px 64px }
}
@media (prefers-reduced-motion:reduce) { * { transition:none!important } }
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
  var VOTE_SUFFIX = '__VOTE_SUFFIX__';
  var esc = function (s) { return String(s == null ? '' : s).replace(/[&<>\"]/g, function (c) {
    return {'&':'&amp;','<':'&lt;','>':'&gt;','\"':'&quot;'}[c];
  }); };
  var label = function (s) { return String(s).replace(/_/g, ' ').replace(/\\b\\w/g, function (c) { return c.toUpperCase(); }); };
  var counts = function (nodes) { var result = {}; ORDER.forEach(function (s) { result[s] = 0; });
    nodes.forEach(function (n) { result[n.s] = (result[n.s] || 0) + 1; }); return result; };
  var ALL = counts(NODES);
  var state = { q:'', status:new Set(), projects:new Set(), projectFilterActive:false, group:'', prio:'', size:'', from:'', ty:'', planOnly:false, prOnly:false, demand:false };
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
  var statRows = [['Total', NODES.length, ''], ['In progress', ALL.in_progress || 0, 'is-prog'],
    ['In review', ALL.in_review || 0, ''], ['Ready', ALL.ready || 0, 'is-ready'], ['Blocked', ALL.blocked || 0, 'is-blocked'],
    ['Design', ALL.design || 0, ''], ['Idea', ALL.idea || 0, ''], ['Deferred', ALL.deferred || 0, ''],
    ['Done', ALL.done || 0, 'is-done'], ['Shipped', NODES.length ? Math.round(100 * (ALL.done || 0) / NODES.length) + '%' : '0%', 'is-done']];
  statRows.forEach(function (s) { var d = document.createElement('div'); d.className = 'stat' + (s[2] ? ' ' + s[2] : '');
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
  var UNSCOPED = DATA.unscoped_label;
  var COLORS = DATA.status_colors || {};
  // Null-prototype: these are keyed on GRAPH values, so a type or status
  // named `constructor` or `toString` would otherwise inherit an Object
  // member and put a function in a class attribute.
  var TYPE_CLASSES = Object.assign(Object.create(null), DATA.type_classes || {});
  var TYPE_FALLBACK = DATA.type_fallback;
  var UNBADGED = DATA.unbadged_type;
  // feature is the overwhelming majority, so it gets no badge; badging it
  // would put a chip on 87 percent of rows and say nothing.
  function typeBadge(t) {
    if (!t || t === UNBADGED) return '';
    return '<span class=\"pill ' + (TYPE_CLASSES[t] || TYPE_FALLBACK) + '\">' + esc(t) + '</span>';
  }
  var UNKNOWN_COLOR = DATA.unknown_color;
  projectNames.sort(function (a, b) { return a === UNSCOPED ? 1 : b === UNSCOPED ? -1 : a.localeCompare(b); });
  var projectChips = document.getElementById('projectChips');
  projectNames.forEach(function (p) { var b = document.createElement('button'); b.className = 'chip project-chip'; b.type = 'button';
    b.dataset.project = p; b.textContent = p; b.setAttribute('aria-pressed', 'false'); b.addEventListener('click', function () {
      if (state.projects.has(p)) state.projects.delete(p); else state.projects.add(p);
      state.projectFilterActive = state.projects.size > 0; saveProjects(); syncProjects(); render(); }); projectChips.appendChild(b); });
  var loadedProjects = loadProjects();
  // Intersect against the projects actually on THIS board. The key is shared
  // by every board on the origin, so a selection saved on the global board
  // names projects a scoped board has never heard of; without this those
  // match nothing, every row hides, and no chip renders pressed to explain it.
  var present = new Set(projectNames);
  state.projects = new Set(Array.from(loadedProjects.selected).filter(function (p) { return present.has(p); }));
  state.projectFilterActive = loadedProjects.active && state.projects.size > 0;
  var queryProject = new URLSearchParams(window.location.search).get('project');
  if (queryProject && projectNames.indexOf(queryProject) >= 0) { state.projects = new Set([queryProject]); state.projectFilterActive = true; }
  function syncProjects() { projectChips.querySelectorAll('[data-project]').forEach(function (b) {
    b.setAttribute('aria-pressed', state.projects.has(b.dataset.project) ? 'true' : 'false'); }); }
  syncProjects();
  var groups = []; NODES.forEach(function (n) { if (groups.indexOf(n.g) < 0) groups.push(n.g); }); groups.sort();
  var groupSel = document.getElementById('groupSel'); groups.forEach(function (g) { var o = document.createElement('option'); o.value = g; o.textContent = g; groupSel.appendChild(o); });
  groupSel.addEventListener('change', function () { state.group = groupSel.value; render(); });
  function fill(sel, key, prefix) { var vals = []; NODES.forEach(function (n) { if (n[key] && vals.indexOf(n[key]) < 0) vals.push(n[key]); }); vals.sort(); vals.forEach(function (v) { var o = document.createElement('option'); o.value = v; o.textContent = prefix ? prefix + ' ' + v : v; sel.appendChild(o); }); }
  fill(document.getElementById('typeSel'), 'ty', '');
  document.getElementById('typeSel').addEventListener('change', function (e) { state.ty = e.target.value; render(); });
  fill(document.getElementById('prioSel'), 'p', 'Priority'); fill(document.getElementById('sizeSel'), 'sz', 'Size');
  document.getElementById('prioSel').addEventListener('change', function (e) { state.prio = e.target.value; render(); });
  document.getElementById('sizeSel').addEventListener('change', function (e) { state.size = e.target.value; render(); });
  var fromEl = document.getElementById('fromDate'); var stamps = NODES.map(function (n) { return n.u || n.c || ''; }).filter(Boolean).sort();
  if (stamps.length) { fromEl.min = stamps[0]; fromEl.max = stamps[stamps.length - 1]; }
  fromEl.addEventListener('change', function () { state.from = fromEl.value || '';
    document.getElementById('datef').className = 'datef' + (state.from ? ' on' : ''); render(); });
  document.getElementById('q').addEventListener('input', function (e) { state.q = e.target.value.toLowerCase().trim(); render(); });
  function buttonFilter(id, key) { var b = document.getElementById(id); if (!b) return; b.addEventListener('click', function () { state[key] = !state[key]; b.setAttribute('aria-pressed', state[key] ? 'true' : 'false'); render(); }); }
  buttonFilter('planOnly', 'planOnly'); buttonFilter('prOnly', 'prOnly');
  if (LOCAL) buttonFilter('demandOnly', 'demand');
  var openStatuses = new Set(ORDER.filter(function (s) { return s !== 'superseded' && (s !== 'done' || DATA.initial_done); })); openStatuses.forEach(function (s) { state.status.add(s); });
  ORDER.forEach(function (s) { var b = statusChips.querySelector('[data-s="' + s + '"]'); if (b) b.setAttribute('aria-pressed', state.status.has(s) ? 'true' : 'false'); });
  function projectMatch(n) { return !state.projectFilterActive || !state.projects.size || state.projects.has(n.project); }
  function matches(n) {
    if (state.demand && !n.en) return false;
    if (!projectMatch(n) || (state.status.size && !state.status.has(n.s))) return false;
    if (state.group && state.group !== n.g) return false; if (state.prio && state.prio !== n.p) return false;
    if (state.size && state.size !== n.sz) return false;
    if (state.ty && state.ty !== n.ty) return false; if (state.from && (n.s === 'done' || n.s === 'superseded') && (n.u || n.c || '') < state.from) return false;
    if (state.planOnly && !(n.pl && n.s !== 'done' && n.s !== 'superseded')) return false; if (state.prOnly && !n.pr) return false;
    if (state.q && (String(n.id || '') + ' ' + n.t + ' ' + String(n.d || '') + ' ' + String(n.pl || '')).toLowerCase().indexOf(state.q) < 0) return false;
    return true;
  }
  // Copy, with the execCommand fallback the canonical template carried. This
  // board is opened from disk as often as over http, and file:// is not a
  // secure context, so navigator.clipboard is frequently absent exactly where
  // the operator uses it most. Dropping the fallback would make the buttons
  // dead on the surface they matter on.
  function flash(btn, msg) {
    var prev = btn.dataset.label || btn.textContent;
    btn.dataset.label = prev;
    btn.textContent = msg; btn.classList.add('ok');
    setTimeout(function () { btn.textContent = prev; btn.classList.remove('ok'); }, 1200);
  }
  function legacyCopy(text) {
    try {
      var ta = document.createElement('textarea');
      ta.value = text; ta.setAttribute('readonly', '');
      ta.style.position = 'fixed'; ta.style.top = '-1000px'; ta.style.opacity = '0';
      document.body.appendChild(ta); ta.select(); ta.setSelectionRange(0, ta.value.length);
      var ok = document.execCommand('copy');
      document.body.removeChild(ta);
      return ok;
    } catch (e) { return false; }
  }
  function copyText(text, btn) {
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(
          function () { flash(btn, 'copied'); },
          function () { flash(btn, legacyCopy(text) ? 'copied' : 'select it'); }
        );
        return;
      }
    } catch (e) {}
    flash(btn, legacyCopy(text) ? 'copied' : 'select it');
  }
  var TERMINAL = Object.assign(Object.create(null), { done:1, superseded:1 });
  // A parent's rollup, in the same stacked-bar language the group headers use.
  // Reusing .tw rather than authoring a second progress idiom.
  function kidBar(n) {
    var kids = n.ki || [];
    if (!kids.length) return '';
    var open = kids.filter(function (k) { return !TERMINAL[k.s]; }).length;
    var pct = 100 * (kids.length - open) / kids.length;
    return '<span class=\"kids\" title=\"' + open + ' of ' + kids.length + ' children open\">'
      + '<span class=\"tw\"><i style=\"width:' + pct + '%;background:' + (COLORS.done || '') + '\"></i>'
      + '<i style=\"width:' + (100 - pct) + '%;background:' + (COLORS.in_progress || '') + '\"></i></span>'
      + open + '/' + kids.length + '</span>';
  }
  // Every RENDERED node is in the DOM, so a child link is an in-page anchor.
  // No server, no second page, and it works on a board opened from disk.
  // Relations are built from the whole graph while the board renders a subset,
  // so an id with no row must NOT become a link: the anchor would set the hash
  // and reveal nothing, which reads as a broken page rather than as absent work.
  var IDS = Object.create(null);
  NODES.forEach(function (n) { IDS[n.id] = 1; });
  function nodeLink(id, label) {
    if (!IDS[id]) return '<b class=\"nid\">' + esc(label || id) + '</b>';
    return '<a class=\"nid\" href=\"#' + esc(id) + '\">' + esc(label || id) + '</a>';
  }
  function relatedBlock(cls, heading, items) {
    if (!items || !items.length) return '';
    return '<div class=\"blk ' + cls + '\"><div class=\"h\">' + heading + '</div>'
      + items.map(function (b) {
          var st = b.s || '';
          return '<div class=\"item\">' + nodeLink(b.id)
            + ' <span class=\"st' + (st === 'not found' ? ' stale' : '') + '\">' + esc(st) + '</span> '
            + esc(b.t || '') + '</div>';
        }).join('')
      + '</div>';
  }
  function voteCommand(n) { return 'fno backlog encounter ' + n.id + VOTE_SUFFIX; }
  function detail(n) {
    var h = '<div class=\"kv\">';
    // The row's .rid copy is mouse-only by construction: it is a span inside
    // .rmain's button, and a button cannot nest one. So the keyboard path to
    // the same copy is this button, reached by expanding the row.
    if (LOCAL) h += '<span><b>id</b> ' + esc(n.id) + ' <button class=\"pbtn\" type=\"button\" data-copy=\"id\">Copy</button></span>';
    h += '<span><b>status</b> ' + esc(n.s) + '</span>' + (n.p ? '<span><b>priority</b> ' + esc(n.p) + '</span>' : '') + (n.sz ? '<span><b>size</b> ' + esc(n.sz) + '</span>' : '') + '</div>';
    if (LOCAL && n.en) h += '<div class="kv"><span><b>encounters</b> ' + n.en + ' (' + (n.en - n.eo) + ' agent, ' + n.eo + ' operator)</span><button class="pbtn" type="button" data-copy="vote">Copy upvote</button></div>';
    if (n.pa) h += '<div class=\"blk kin\"><div class=\"h\">Parent</div><div class=\"item\">'
      // No not-found marker here: pt_ is empty BOTH when the parent is absent
      // from the graph and when it simply has no title, so a marker keyed on it
      // calls a live parent missing. nodeLink already declines to link an id
      // with no row, which is the honest signal available at this point.
      + nodeLink(n.pa) + (n.pt_ ? ' ' + esc(n.pt_) : '') + '</div></div>';
    h += relatedBlock('kin', 'Children', n.ki);
    h += relatedBlock('', 'Dependencies', n.bb);
    h += relatedBlock('kin', 'Unblocks', n.su);
    if (n.sb) h += relatedBlock('kin', 'Superseded by', [n.sb]);
    if (n.d) h += '<p>' + esc(n.d) + '</p>'; else if (LOCAL) h += '<p>No description recorded.</p>';
    if (LOCAL && n.pl) h += '<div class=\"planrow\"><span class=\"plan\">' + esc(n.pl) + '</span>' + '<button class=\"pbtn\" type=\"button\" data-copy=\"path\">Copy path</button>' + (n.link ? '<a class=\"pbtn primary\" href=\"' + esc(n.link) + '\">Open in Obsidian ↗</a>' : '') + '</div>';
    if (LOCAL && n.pr) h += '<div class=\"planrow\">' + (n.pu ? '<a class=\"pbtn primary\" href=\"' + esc(n.pu) + '\">PR #' + esc(n.pr) + '</a>' : '<span class=\"pill\">PR #' + esc(n.pr) + '</span>') + (n.pt ? '<span class=\"plan\">' + esc(n.pt) + '</span>' : '') + '</div>';
    return h;
  }
  var sections = [];
  function build() {
    var board = document.getElementById('board'); board.innerHTML = '';
    var by = {}; NODES.forEach(function (n) { (by[n.g] = by[n.g] || []).push(n); });
    Object.keys(by).sort().forEach(function (g) { var rows = by[g];
      var sec = document.createElement('section'); sec.className = 'group';
      var head = document.createElement('button'); head.className = 'ghead'; head.type = 'button';
      var caret = document.createElement('span'); caret.className = 'caret'; caret.textContent = '\u25bc';
      var title = document.createElement('h2'); title.textContent = g;
      var bar = document.createElement('span'); bar.className = 'tw';
      var count = document.createElement('span'); count.className = 'gc';
      head.appendChild(caret); head.appendChild(title); head.appendChild(bar); head.appendChild(count);
      var list = document.createElement('div'); list.className = 'rows';
      sec.dataset.open = 'true';
      head.addEventListener('click', function () {
        sec.dataset.open = sec.dataset.open === 'false' ? 'true' : 'false'; });
      sec.appendChild(head);
      var built = rows.map(function (n) { var row = document.createElement('div'); row.className = 'row';
        if (n.id) row.id = n.id;
        row.dataset.project = n.project; row.dataset.s = n.s;
        if (n.ty) row.dataset.type = n.ty;
        var main = document.createElement('button'); main.className = 'rmain'; main.type = 'button';
        var votePill = LOCAL && n.en ? '<span class=\"votes\" title=\"' + (n.en - n.eo) + ' agent, ' + n.eo + ' operator\">▲ ' + n.en + '</span>' : '';
        main.innerHTML = (LOCAL ? '<span class=\"rid\">' + esc(n.id) + '</span>' : '<span class=\"rid\"></span>') + '<span class=\"rt\">' + esc(n.t) + '</span><span class=\"meta\">' + typeBadge(n.ty) + '<span class=\"pill s-' + esc(n.s) + '\">' + esc(n.s) + '</span>' + (n.p ? '<span class=\"pill' + (n.p === 'p0' || n.p === 'p1' ? ' pr-p1' : '') + '\">' + esc(n.p) + '</span>' : '') + (n.sz ? '<span class=\"pill\">' + esc(n.sz) + '</span>' : '') + '</span><span class=\"dot\">' + kidBar(n) + votePill + (n.pl ? '<span class=\"haspl\">plan</span>' : '') + (n.pr ? '<span class=\"haspr\">PR</span>' : '') + esc(n.u || n.c || '') + '</span>';
        main.setAttribute('aria-expanded', 'false');
        // The id is the thing most often copied out of this board, so it is
        // one click ON the id rather than a trip through the detail. A span,
        // not a button, because .rmain is already a button and nesting one is
        // invalid; stopPropagation keeps the copy from toggling the row. A span
        // cannot take focus, so keyboard users copy the id from the detail's
        // Copy button instead; this is the pointer shortcut to it.
        if (LOCAL && n.id) { var rid = main.querySelector('.rid');
          if (rid) { rid.title = 'Copy node id';
            rid.addEventListener('click', function (ev) { ev.stopPropagation(); copyText(n.id, rid); }); } }
        if (LOCAL && n.en) { var votes = main.querySelector('.votes');
          if (votes) votes.addEventListener('click', function (ev) { ev.stopPropagation(); copyText(voteCommand(n), votes); }); }
        main.addEventListener('click', function () { var old = row.querySelector('.detail');
          if (old) { old.remove(); main.setAttribute('aria-expanded', 'false'); return; }
          var d = document.createElement('div'); d.className = 'detail'; d.innerHTML = detail(n);
          d.querySelectorAll('[data-copy]').forEach(function (b) {
            b.addEventListener('click', function (ev) { ev.stopPropagation();
              copyText(b.dataset.copy === 'id' ? n.id : b.dataset.copy === 'vote' ? voteCommand(n) : n.pl, b); });
          });
          row.appendChild(d); main.setAttribute('aria-expanded', 'true'); });
        row.appendChild(main); list.appendChild(row); return { node:n, el:row }; });
      sec.appendChild(list); board.appendChild(sec);
      sections.push({ el:sec, bar:bar, count:count, rows:built });
    });
  }
  // An anchored row is exempt from the filter for exactly one render: its own.
  // Clearing is-hidden directly did not survive, because render() reassigns
  // className wholesale. Keeping the exemption FOREVER is the opposite defect:
  // the row then counts toward every later group header and total, which is the
  // same lie the visible-count invariant below exists to prevent. So the next
  // render the reader causes drops it, and the row obeys the filter again.
  var revealed = null;
  function render(keepReveal) {
    if (!keepReveal) revealed = null;
    var shown = 0;
    sections.forEach(function (sec) {
      var vis = [];
      // build() fixed the DOM order and render() only toggles classes, so a demand
      // sort has to move nodes. sec.rows keeps authored order, so toggling off
      // re-appends in that order instead of leaving the board sorted.
      var order = state.demand ? sec.rows.slice().sort(function (a, b) { return (b.node.dv || 0) - (a.node.dv || 0); }) : sec.rows;
      order.forEach(function (r) { r.el.parentNode.appendChild(r.el); });
      order.forEach(function (r) { var ok = matches(r.node) || r.node.id === revealed;
        r.el.className = 'row' + (ok ? '' : ' is-hidden'); if (ok) vis.push(r.node); });
      shown += vis.length;
      // Header count and stacked bar describe what is VISIBLE. Reading them off
      // the unfiltered group made thirteen headers claim the whole graph while
      // the board showed three rows, and painted all-filtered groups as empty
      // sections. The rows stay in the DOM either way.
      var c = counts(vis);
      sec.count.textContent = vis.length;
      sec.bar.innerHTML = vis.length ? ORDER.map(function (s) {
        if (!c[s]) return '';
        var color = COLORS[s] || UNKNOWN_COLOR;
        return '<i style=\"width:' + (100 * c[s] / vis.length) + '%;background:' + color + '\"></i>';
      }).join('') : '';
      sec.el.className = 'group' + (vis.length ? '' : ' is-hidden');
    });
    document.getElementById('shown').textContent = shown + ' of ' + NODES.length + ' nodes shown';
  }
  // A child link can name a row the active filter hides, and an anchor to a
  // display:none element scrolls nowhere. Reveal the target, open its group,
  // and expand it, so the link always lands somewhere visible.
  function revealHash() {
    var id = (location.hash || '').slice(1);
    if (!id) return;
    var row = document.getElementById(id);
    // Only a ROW is a reveal target. `#board` and the control ids also resolve,
    // and expanding whatever .rmain they happen to contain is not what the
    // reader asked for.
    if (!row || !row.classList.contains('row')) return;
    revealed = id;
    render(true);
    var sec = row.closest('.group');
    if (sec) { sec.classList.remove('is-hidden'); sec.dataset.open = 'true'; }
    var main = row.querySelector('.rmain');
    if (main && main.getAttribute('aria-expanded') !== 'true') main.click();
    row.scrollIntoView({ block: 'center' });
  }
  window.addEventListener('hashchange', revealHash);
  build();
  render();
  revealHash();
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
    if local:
        from fno.graph._intake import make_effective_priority
        from fno.graph.demand import divergence_score, voter_key

        priority_for = make_effective_priority(source)
    index = {e.get("id"): e for e in source if isinstance(e.get("id"), str)}
    # Successors, indexed once. Scanning `source` per entry to find what each
    # one unblocks is quadratic, and this runs inside the auto-render hook on
    # EVERY graph mutation: 4694 entries cost 3.26s that way against 0.07s
    # here, to populate 424 rows. Board freshness is the point of this file.
    # Children, indexed once, for the same reason successors are: scanning the
    # source per entry is quadratic and this runs on every graph mutation.
    # Keyed on `parent`, NOT on type == epic: 53 of 85 parents measured in the
    # fno project are ordinary nodes, so an epic-only index misses most of it.
    kids: dict[str, list[dict]] = {}
    for child in source:
        parent = child.get("parent")
        if isinstance(parent, str) and parent:
            kids.setdefault(parent, []).append(child)

    successors: dict[str, list[dict]] = {}
    for successor in source:
        if not isinstance(successor.get("id"), str):
            continue
        for blocker in successor.get("blocked_by") or []:
            if isinstance(blocker, str):
                successors.setdefault(blocker, []).append(successor)
    rows: list[dict] = []
    for entry in entries:
        status = _row_status(entry)
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
            # Type is safe to publish: it names a kind of work, not an
            # identifier. `parent` is a node id, so it stays local-only below.
            "ty": str(entry.get("type") or ""),
        }
        if local:
            voters = {
                voter_key(encounter)
                for encounter in (entry.get("encounters") or [])
                if isinstance(encounter, dict) and voter_key(encounter)
            }
            operator_voters = {
                voter_key(encounter)
                for encounter in (entry.get("encounters") or [])
                if isinstance(encounter, dict)
                and encounter.get("voter_kind") == "operator"
                and voter_key(encounter)
            }
            row.update(
                {
                    "id": str(entry.get("id") or "?"),
                    "pa": (
                        str(entry.get("parent"))
                        if isinstance(entry.get("parent"), str) and entry.get("parent")
                        else ""
                    ),
                    "pt_": (
                        str(index.get(str(entry.get("parent")), {}).get("title") or "")[:90]
                        if isinstance(entry.get("parent"), str)
                        else ""
                    ),
                    "ki": [
                        {
                            "id": str(child.get("id") or "?"),
                            "s": _row_status(child),
                            "t": str(child.get("title") or "")[:90],
                            "ty": str(child.get("type") or ""),
                        }
                        for child in kids.get(str(entry.get("id") or ""), ())
                    ],
                    "d": " ".join(str(entry.get("details") or "").split()),
                    "pl": str(entry.get("plan_path") or ""),
                    "pr": str(entry.get("pr_number") or ""),
                    # Only an http(s) url becomes an anchor; a scheme-less one
                    # would be a relative link. It still travels as "pt" so the
                    # detail shows it as text rather than silently losing it.
                    "pu": (
                        str(entry.get("pr_url") or "")
                        if str(entry.get("pr_url") or "").startswith(
                            ("https://", "http://")
                        )
                        else ""
                    ),
                    "pt": (
                        ""
                        if str(entry.get("pr_url") or "").startswith(
                            ("https://", "http://")
                        )
                        else str(entry.get("pr_url") or "")
                    ),
                    "su": [
                        {
                            "id": str(successor.get("id") or "?"),
                            "s": _row_status(successor),
                            "t": str(successor.get("title") or "")[:90],
                        }
                        for successor in successors.get(str(entry.get("id") or ""), ())
                    ],
                    "sb": (
                        {
                            "id": str(entry.get("superseded_by")),
                            "s": _row_status(
                                index.get(entry.get("superseded_by")), "not found"
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
                            "s": _row_status(index.get(bid), "not found"),
                            "t": str(index.get(bid, {}).get("title") or "")[:90],
                        }
                        for bid in entry.get("blocked_by") or []
                        if isinstance(bid, str)
                    ],
                }
            )
            if voters:
                operator_count = len(voters & operator_voters)
                row.update(
                    {
                        "en": len(voters),
                        "eo": operator_count,
                        "dv": divergence_score(entry, priority_for(entry)),
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
        # `.blk` alone is the BLOCKED box (red border and fill). Only Dependencies
        # earn that; a `kin` modifier neutralises it for everything else. The
        # no-JS board is a real reading surface, so it uses the same vocabulary
        # the scripted board does rather than a bare class that now means "red".
        for heading, key, cls in (
            ("Children", "ki", "blk kin"),
            ("Dependencies", "bb", "blk"),
            ("Unblocks", "su", "blk kin"),
        ):
            related = row.get(key) or []
            if not isinstance(related, list) or not related:
                continue
            parts.append(f'<div class="{cls}"><div class="h">{heading}</div>')
            for item in related:
                if not isinstance(item, dict):
                    continue
                status = str(item.get("s") or "unknown")
                stale = " stale" if status == "not found" else ""
                parts.append(
                    f'<div class="item"><b class="nid">'
                    f'{html.escape(str(item.get("id") or "?"))}</b> '
                    f'<span class="st{stale}">{html.escape(status)}</span> '
                    f'{html.escape(str(item.get("t") or ""))}</div>'
                )
            parts.append("</div>")
        if row.get("d"):
            parts.append(f'<p>{html.escape(str(row["d"]))}</p>')
        if row.get("pl"):
            parts.append(
                f'<div class="planrow"><span class="plan">{html.escape(str(row["pl"]))}</span></div>'
            )
        if row.get("pr"):
            # Same anchor guard as the scripted detail: only an http(s) url is
            # linkified, a scheme-less one stays visible as escaped text.
            number = html.escape(str(row["pr"]))
            link = str(row.get("pu") or "")
            badge = (
                f'<a class="pbtn primary" href="{html.escape(link, quote=True)}">PR #{number}</a>'
                if link
                else f'<span class="pill">PR #{number}</span>'
            )
            text = (
                f'<span class="plan">{html.escape(str(row["pt"]))}</span>'
                if row.get("pt")
                else ""
            )
            parts.append(f'<div class="planrow">{badge}{text}</div>')
    parts.append("</div>")
    return "".join(parts)


def _dashboard_static_html(
    rows: list[dict], *, local: bool, initial_done: bool = False
) -> str:
    """The no-JS board, showing the SAME rows the scripted board shows on
    first paint rather than every row in the graph.

    Every node was being serialised twice, once here and once as JSON, and
    build() discards this half the moment a script runs. Measured on the real
    graph: 10.1 MB of an 18.7 MB document, nearly all of it closed work the
    script hides anyway. A reader with JS sees an identical board either way;
    a reader without it now sees what the chips would have shown.
    """
    hidden = tuple(
        status
        for status in _DASHBOARD_TERMINAL_STATUSES
        if not (initial_done and status == "done")
    )
    groups: dict[str, list[dict]] = {}
    for row in rows:
        if str(row.get("s") or "") in hidden:
            continue
        groups.setdefault(str(row.get("g") or "uncategorized"), []).append(row)
    if not groups:
        return '<p class="empty">No open nodes.</p>'
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
                f'<div class="row" data-project="{project}" '
                # `data-s` is what the de-emphasis rules read; the scripted
                # builder sets it and the static half did not, so closed work
                # rendered at full weight until the script replaced the board.
                f'data-s="{html.escape(str(row.get("s") or ""), quote=True)}">'
                f'<div class="rmain"><span class="rid">{node_id}</span>'
                f'<span class="rt">{html.escape(str(row.get("t") or ""))}</span>'
                f'<span class="meta"><span class="pill {_dashboard_status_class(row.get("s"))}">'
                f'{html.escape(str(row.get("s") or ""))}</span></span>'
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
    projection: str = "backlog",
) -> str:
    rows = _dashboard_rows(
        entries, local=local, vault=vault, context_entries=context_entries
    )
    payload = json.dumps(
        {
            "nodes": rows,
            "status_order": list(_DASHBOARD_STATUS_ORDER),
            "unscoped_label": UNSCOPED_LABEL,
            "status_colors": _DASHBOARD_STATUS_COLORS,
            "type_classes": _DASHBOARD_TYPE_CLASSES,
            "type_fallback": _DASHBOARD_TYPE_FALLBACK,
            "unbadged_type": _DASHBOARD_UNBADGED_TYPE,
            "unknown_color": _DASHBOARD_UNKNOWN_COLOR,
            # A roadmap's whole point is the shipped column, so it opens with
            # done pressed. Every other surface opens on open work.
            "initial_done": projection == "roadmap",
        },
        separators=(",", ":"),
    ).replace("&", "\\u0026").replace("<", "\\u003c").replace(">", "\\u003e")
    generated = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    # The original page said "snapshot" here, honestly, because a vault script
    # rendered it and nothing re-ran that script. A LOCAL board is written by
    # the auto-render hook on every graph mutation, so it says so. A published
    # board is not: it is a snapshot at publish time, and claiming otherwise on
    # the surface strangers read is the same dishonesty this note replaced.
    # Keyed on `local`, NOT on projection: render_graph_html passes local=True
    # and leaves projection at its default, so keying on projection labels the
    # operator's live board a snapshot, which is the same lie pointed the other
    # way. A local board is written by the auto-render hook on every mutation;
    # a published one is a snapshot taken when it was published.
    if local:
        scope_note = "live \u00b7 re-rendered on every graph mutation"
        opens_note = "Opens on the live board. "
        # Description, blockers and the plan link are added under `if local:`
        # in _dashboard_rows. Promising them on a public board sends a reader
        # to a detail pane holding only the status their pill already showed.
        detail_note = "Click any row for its description, blockers and plan link."
    else:
        scope_note = "snapshot \u00b7 rendered when published"
        # roadmap opens with `done` pressed, so it does not open on live work.
        opens_note = "" if projection == "roadmap" else "Opens on open work. "
        detail_note = "Click any row for its status and dates."
    # Built outside the f-string: an escape inside an f-string EXPRESSION
    # is Python 3.12+, and this package targets 3.11.
    status_legend = " \u00b7 ".join(_DASHBOARD_STATUS_ORDER)
    static_board = _dashboard_static_html(
        rows, local=local, initial_done=projection == "roadmap"
    )
    vote_suffix = (
        ' --operator --evidence "REPLACE: what it cost"' if local else ""
    )
    dashboard_js = _DASHBOARD_JS.replace("__VOTE_SUFFIX__", vote_suffix)
    # The font is the original design's, so it stays; the LOAD is what must not
    # block. A stylesheet in <head> is render-blocking, and a network that
    # blackholes rather than refuses (captive portal, offline laptop, locked-down
    # VPN) holds first paint for the full connect timeout. media="print" makes it
    # non-blocking; onload promotes it once it has actually arrived, so the
    # fallback stack paints immediately and the design arrives when it can.
    # This is the ONLY thing here that reaches the network, and the board stays
    # readable with no network at all.
    font_href = (
        "https://fonts.googleapis.com/css2?"
        "family=IBM+Plex+Mono:wght@400;500;600&"
        "family=IBM+Plex+Sans:wght@400;500;600;700&display=swap"
    )
    fonts = (
        '<link rel="preconnect" href="https://fonts.googleapis.com">'
        '<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>'
        f'<link rel="stylesheet" href="{font_href}" media="print" '
        "onload=\"this.media='all';this.onload=null\">"
        f'<noscript><link rel="stylesheet" href="{font_href}"></noscript>'
    )
    return (
        '<!doctype html><html lang="en"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        '<meta name="color-scheme" content="light dark">'
        f"<title>{html.escape(title)}</title>{fonts}"
        f"<style>{_DASHBOARD_CSS}</style></head>"
        f'<body data-local="{str(local).lower()}"><div class="wrap">'
        f'<header><div class="eyebrow">fno \u00b7 generated <span>{generated}</span>'
        f' \u00b7 <span>{html.escape(scope_note)}</span></div>'
        f"<h1>{html.escape(title)}</h1>"
        '<p class="lede">Every open node, plus recently closed work for context. '
        f'<b id="totalCount">{len(rows)}</b> nodes. {opens_note}'
        '<b>Plan, unfinished</b> is the real queue: every node with a plan that has '
        f'not shipped. Set <b>from</b> to a date to narrow the window. {detail_note}</p></header>'
        '<div class="stats" id="stats"></div><div class="controls">'
        '<input type="search" id="q" placeholder="Search title, id, or description\u2026" aria-label="Search nodes">'
        '<div class="chips" id="statusChips" role="group" aria-label="Filter by status"></div>'
        '<div class="chips" id="projectChips" role="group" aria-label="Filter by project"></div>'
        '<select id="groupSel" aria-label="Filter by group"><option value="">All groups</option></select>'
        '<select id="typeSel" aria-label="Filter by type"><option value="">Any type</option></select>'
        '<label class="datef" id="datef">from <input type="date" id="fromDate" aria-label="Show work touched on or after this date"></label>'
        '<select id="prioSel" aria-label="Filter by priority"><option value="">Any priority</option></select>'
        '<select id="sizeSel" aria-label="Filter by size"><option value="">Any size</option></select>'
        '<button class="chip" id="planOnly" type="button" aria-pressed="false">Plan, unfinished <span class="c" id="planCount"></span></button>'
        '<button class="chip" id="prOnly" type="button" aria-pressed="false">has a PR <span class="c" id="prCount"></span></button>'
        + (
            '<button class="chip" id="demandOnly" type="button" aria-pressed="false">Demand</button>'
            if local
            else ""
        )
        + f'</div><main id="board">{static_board}</main>'
        f'<footer><span id="shown"></span><span>rendered {generated}</span>'
        f"<span>statuses: {status_legend}</span></footer>"
        f'</div><script id="data" type="application/json">{payload}</script>'
        f"<script>{dashboard_js}</script></body></html>\n"
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
    return _dashboard_html(entries, title=title, local=False, projection=projection)
