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
_DASHBOARD_STATUS_COLORS = {
    "in_progress": "var(--prog)",
    "in_review": "var(--prog)",
    "ready": "var(--accent)",
    "blocked": "var(--blocked)",
    "design": "var(--idea)",
    "idea": "var(--idea)",
    "deferred": "#8c929a",
    "done": "var(--done)",
    "superseded": "#a5a9af",
}
_DASHBOARD_UNKNOWN_COLOR = "#b9bec4"

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
.gc { color:var(--muted); font-size:12px }
.rows { border-top:1px solid var(--line) }
.row { border-bottom:1px solid var(--line) }
.row:last-child { border-bottom:0 }
.row.is-hidden, .group.is-hidden { display:none }
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
  var state = { q:'', status:new Set(), projects:new Set(), projectFilterActive:false, group:'', prio:'', size:'', from:'', planOnly:false, prOnly:false };
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
  var UNSCOPED = DATA.unscoped_label;
  var COLORS = DATA.status_colors || {};
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
  var openStatuses = new Set(ORDER.filter(function (s) { return s !== 'superseded' && (s !== 'done' || DATA.initial_done); })); openStatuses.forEach(function (s) { state.status.add(s); });
  ORDER.forEach(function (s) { var b = statusChips.querySelector('[data-s="' + s + '"]'); if (b) b.setAttribute('aria-pressed', state.status.has(s) ? 'true' : 'false'); });
  function projectMatch(n) { return !state.projectFilterActive || !state.projects.size || state.projects.has(n.project); }
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
      head.addEventListener('click', function () { list.hidden = !list.hidden; caret.textContent = list.hidden ? '\u25b6' : '\u25bc'; });
      sec.appendChild(head);
      var built = rows.map(function (n) { var row = document.createElement('div'); row.className = 'row';
        row.dataset.project = n.project; row.dataset.status = n.s;
        var main = document.createElement('button'); main.className = 'rmain'; main.type = 'button'; main.innerHTML = (LOCAL ? '<span class=\"rid\">' + esc(n.id) + '</span>' : '<span class=\"rid\"></span>') + '<span class=\"rt\">' + esc(n.t) + '</span><span class=\"meta\"><span class=\"pill\">' + esc(n.s) + '</span>' + (n.p ? '<span class=\"pill\">' + esc(n.p) + '</span>' : '') + (n.sz ? '<span class=\"pill\">' + esc(n.sz) + '</span>' : '') + '</span><span class=\"dot\">' + (n.pl ? '<span class=\"haspl\">plan</span>' : '') + (n.pr ? '<span class=\"haspr\">PR</span>' : '') + esc(n.u || n.c || '') + '</span>';
        main.setAttribute('aria-expanded', 'false');
        main.addEventListener('click', function () { var old = row.querySelector('.detail');
          if (old) { old.remove(); main.setAttribute('aria-expanded', 'false'); return; }
          var d = document.createElement('div'); d.className = 'detail'; d.innerHTML = detail(n);
          row.appendChild(d); main.setAttribute('aria-expanded', 'true'); });
        row.appendChild(main); list.appendChild(row); return { node:n, el:row }; });
      sec.appendChild(list); board.appendChild(sec);
      sections.push({ el:sec, bar:bar, count:count, rows:built });
    });
  }
  function render() {
    var shown = 0;
    sections.forEach(function (sec) {
      var vis = [];
      sec.rows.forEach(function (r) { var ok = matches(r.node);
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
  build();
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
    # Successors, indexed once. Scanning `source` per entry to find what each
    # one unblocks is quadratic, and this runs inside the auto-render hook on
    # EVERY graph mutation: 4694 entries cost 3.26s that way against 0.07s
    # here, to populate 424 rows. Board freshness is the point of this file.
    successors: dict[str, list[dict]] = {}
    for successor in source:
        if not isinstance(successor.get("id"), str):
            continue
        for blocker in successor.get("blocked_by") or []:
            if isinstance(blocker, str):
                successors.setdefault(blocker, []).append(successor)
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
                            "s": str(successor.get("status") or "unknown"),
                            "t": str(successor.get("title") or "")[:90],
                        }
                        for successor in successors.get(str(entry.get("id") or ""), ())
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
            "unknown_color": _DASHBOARD_UNKNOWN_COLOR,
            # A roadmap's whole point is the shipped column, so it opens with
            # done pressed. Every other surface opens on open work.
            "initial_done": projection == "roadmap",
        },
        separators=(",", ":"),
    ).replace("&", "\\u0026").replace("<", "\\u003c").replace(">", "\\u003e")
    generated = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    static_board = _dashboard_static_html(
        rows, local=local, initial_done=projection == "roadmap"
    )
    return (
        '<!doctype html><html lang="en"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        f"<title>{html.escape(title)}</title><style>{_DASHBOARD_CSS}</style></head>"
        f'<body data-local="{str(local).lower()}"><header class="page"><h1>{html.escape(title)}</h1>'
        '<p class="lede">Every open node, plus recent closed context. '
        f'<b id="totalCount">{len(rows)}</b> nodes.</p>'
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
        f'<footer>rendered {generated}</footer>'
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
    return _dashboard_html(entries, title=title, local=False, projection=projection)
