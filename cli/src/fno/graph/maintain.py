"""Backlog + kanban hygiene sweep for ``fno backlog maintain`` (ab-9c144a4c).

Six legs that keep ``graph.json`` and the kanban board clean by composing
detection logic over the entries list. The CLI command in ``cli.py`` orchestrates
them; this module holds the pure, IO-light detectors so each leg is unit-testable
without a live graph.

Two legs are DETERMINISTIC and apply under ``--apply``:

  1. re-scope  - correct ``project``/``cwd`` drift (project-null, wrong project,
                 or a worktree-path cwd) against the settings workspace map.
                 Only ``project``/``cwd`` are ever changed, never priority/status.
  2. leak-prune - remove nodes whose ``cwd`` is under a temp dir (pytest leaks).

Three legs are JUDGMENT calls and ALWAYS propose-only (never mutate, regardless
of ``--apply``):

  3. dedup  - surface near-duplicate idea titles for human merge/supersede.
  4. drain  - propose a reversible ``defer`` for stale ideas (older than N days).
  5. cap    - report a Now column over its WIP cap; propose triage demotions.

A sixth leg (report) appends a summary to health-history; that lives in the CLI
command since it owns the write target.
"""
from __future__ import annotations

import os
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Optional


# ---------------------------------------------------------------------------
# Workspace map (settings.yaml) - shared with cli/scripts/list_misscoped_graph_nodes.py
# ---------------------------------------------------------------------------

def load_workspaces() -> dict[str, str]:
    """Return ``{project_name: normalized_path}`` from settings.yaml.

    Reads the project-local ``.fno/settings.yaml`` then the global
    ``~/.fno/settings.yaml``, accepting both the multi-workspace
    (``work.workspaces.<ws>.projects[]``) and the legacy flat
    (``work.projects.<name>``) shapes. Best-effort: a missing/malformed file
    contributes nothing rather than raising.
    """
    # Reuse the canonical settings-file resolver (project-local + global,
    # redirect-aware, de-duped) so this map cannot drift from
    # detect_project_from_settings, and so paths route through fno.paths
    # rather than a hardcoded ~/.fno (the no-hardcoded-paths guard).
    from fno.config_io import read_config_flat
    from fno.graph._intake import _settings_candidate_paths

    out: dict[str, str] = {}
    for path in _settings_candidate_paths():
        if not path.exists():
            continue
        # read_config_flat parses config.toml (or a legacy settings.yaml) and
        # returns the FLAT dict, so `work` is top-level.
        work = read_config_flat(path).get("work")
        if not isinstance(work, dict):
            continue
        workspaces = work.get("workspaces")
        if isinstance(workspaces, dict):
            for ws in workspaces.values():
                if not isinstance(ws, dict):
                    continue
                projects = ws.get("projects")
                if not isinstance(projects, list):
                    continue
                for proj in projects:
                    if not isinstance(proj, dict):
                        continue
                    name = proj.get("name")
                    raw = proj.get("path")
                    if isinstance(name, str) and isinstance(raw, str):
                        out[name] = os.path.normpath(os.path.expanduser(raw))
        flat_projects = work.get("projects")
        if isinstance(flat_projects, dict):
            for name, cfg in flat_projects.items():
                if not isinstance(cfg, dict):
                    continue
                raw = cfg.get("path")
                if isinstance(name, str) and isinstance(raw, str):
                    out[name] = os.path.normpath(os.path.expanduser(raw))
    return out


# ---------------------------------------------------------------------------
# Leg 1: re-scope drift
# ---------------------------------------------------------------------------

@dataclass
class RescopeFix:
    """A deterministic project/cwd correction for one node.

    ``new_project``/``new_cwd`` are the canonical values to write. Only these
    two fields are ever touched - never priority or status.
    """

    node_id: str
    old_project: Optional[str]
    new_project: str
    old_cwd: Optional[str]
    new_cwd: str


# Recover the repo-name segment from a worktree cwd of a project-null node (so
# it does not map directly to a canonical workspace path). Three layouts are
# recognized; the caller guards the result with ``hint in workspaces``, so a
# segment that is not a known project simply declines (never mis-scopes):
#   - harness-native (the worktrees_base default, x-33e9):
#       ``<repo>/.claude/worktrees/<name>``        -> ``<repo>``
#   - conductor back-compat (use_conductor_canonical / worktrees_base = conductor):
#       ``.../conductor/workspaces/<repo>/<name>``  -> ``<repo>``
#   - a CUSTOM ``config.paths.worktrees_base`` (passed in by the caller):
#       ``<base>/<repo>/<name>``                    -> ``<repo>``
_CLAUDE_WORKTREE_RE = re.compile(r"/([^/]+)/\.claude/worktrees/")
_CONDUCTOR_WORKTREE_RE = re.compile(r"/conductor/workspaces/([^/]+)/")


def _configured_worktrees_base() -> Optional[str]:
    """Return config.paths.worktrees_base (local then global), or None when unset.

    Lets the rescope hint recognize a node rooted at a CUSTOM worktrees_base
    (``<base>/<repo>/<name>``), closing the AC2 gap for non-default bases (codex
    P1 on PR #67). Reuses the walker's per-file reader so the two stay in sync.
    """
    from fno.graph._intake import _settings_candidate_paths
    from fno.worktree import _read_worktrees_base_from

    for path in _settings_candidate_paths():
        base = _read_worktrees_base_from(path)
        if base is not None:
            return os.path.normpath(os.path.expanduser(base))
    return None


def _worktree_repo_hint(norm_cwd: str, worktrees_base: Optional[str] = None) -> Optional[str]:
    probe = norm_cwd + "/"
    m = _CLAUDE_WORKTREE_RE.search(probe) or _CONDUCTOR_WORKTREE_RE.search(probe)
    if m:
        return m.group(1)
    # Custom configured base: <base>/<repo>/<name> -> <repo>.
    if worktrees_base:
        base = worktrees_base.rstrip("/")
        if norm_cwd.startswith(base + "/"):
            rest = norm_cwd[len(base) + 1:].split("/")
            if len(rest) >= 2 and rest[0] and rest[0] != "..":
                return rest[0]
    return None


def detect_rescope_fixes(
    entries: list[dict], workspaces: dict[str, str]
) -> list[RescopeFix]:
    """Nodes whose ``project``/``cwd`` disagree with the workspace map.

    Generalizes ``list_misscoped_graph_nodes.py`` (which required project AND
    cwd to be set, and only reported) to also catch ``project: null`` and a
    worktree-path cwd, and to emit a concrete fix. Drift shapes handled:

    * project set to a name that maps to a known workspace, but cwd != that
      workspace path (the worktree-cwd case): fix cwd -> canonical.
    * project set to a name NOT in the map, but cwd maps to a known project:
      fix project + cwd to that project.
    * project null, cwd maps to a known project: fix project + cwd.
    * project null, cwd is a conductor worktree whose <repo> is a known
      project: fix project + cwd.

    A node already consistent with the map yields no fix (idempotent). When the
    project cannot be determined the node is left untouched for a human.
    """
    if not workspaces:
        return []
    # path -> project, for reverse lookup of "which project owns this cwd".
    path_to_project = {path: proj for proj, path in workspaces.items()}
    # Resolved once: a custom worktrees_base lets the hint recognize a node
    # rooted at <base>/<repo>/<name> (in addition to harness-native/conductor).
    wt_base = _configured_worktrees_base()

    fixes: list[RescopeFix] = []
    for e in entries:
        node_id = e.get("id")
        if not isinstance(node_id, str):
            continue
        cwd = e.get("cwd")
        proj = e.get("project")
        if not cwd:
            continue  # nothing to anchor a correction on
        norm_cwd = os.path.normpath(os.path.expanduser(str(cwd)))
        candidate = path_to_project.get(norm_cwd)

        target_project: Optional[str] = None
        if isinstance(proj, str) and proj in workspaces:
            # Project known. Only the cwd may have drifted (e.g. a worktree).
            if norm_cwd != workspaces[proj]:
                target_project = proj
        elif candidate is not None:
            # project null or an unknown name, but the cwd maps to a project.
            if candidate != proj:
                target_project = candidate
        elif not proj:
            # project null and cwd does not map directly: try a worktree hint.
            hint = _worktree_repo_hint(norm_cwd, wt_base)
            if hint and hint in workspaces:
                target_project = hint

        if target_project is None:
            continue
        new_cwd = workspaces[target_project]
        # Skip a no-op (already canonical on both fields).
        if proj == target_project and norm_cwd == new_cwd:
            continue
        fixes.append(
            RescopeFix(
                node_id=node_id,
                old_project=proj if isinstance(proj, str) else None,
                new_project=target_project,
                old_cwd=str(cwd),
                new_cwd=new_cwd,
            )
        )
    return fixes


# ---------------------------------------------------------------------------
# Leg 2: leak-prune (pytest test-temp nodes)
# ---------------------------------------------------------------------------

# Markers that identify a pytest/test temp directory specifically. We match on
# these MARKERS, not the bare temp ROOT (/tmp, /var/folders): a legitimate
# project checkout or scratch worktree can live under a temp root (common in CI),
# and pruning is destructive (removes the node), so matching the whole prefix
# would delete real backlog nodes (codex P2 on PR #474). The conftest HOME
# redirect uses ``tempfile.mkdtemp(prefix="fno-test-home-")`` and pytest's
# tmp_path uses ``pytest-of-<user>/pytest-N`` - both carry one of these markers,
# so requiring a marker still catches every real leak while sparing real cwds.
_TEMP_CWD_MARKERS = ("pytest-of-", "/pytest-", "fno-test-home-")


def is_temp_cwd(cwd: object) -> bool:
    """True when ``cwd`` carries a pytest/test-temp MARKER (not just a temp root).

    Requiring a marker rather than matching the bare ``/tmp`` // ``/var/folders``
    prefix keeps a legitimate checkout under a temp root from being pruned.
    """
    if not cwd or not isinstance(cwd, str):
        return False
    norm = os.path.normpath(os.path.expanduser(cwd))
    return any(marker in norm for marker in _TEMP_CWD_MARKERS)


def detect_temp_leaks(entries: list[dict]) -> list[str]:
    """Node ids whose cwd is under a temp dir (test leaks to prune)."""
    return [
        e["id"]
        for e in entries
        if isinstance(e.get("id"), str) and is_temp_cwd(e.get("cwd"))
    ]


# ---------------------------------------------------------------------------
# Leg 3: dedup (propose-only)
# ---------------------------------------------------------------------------

def _normalize_title(title: object) -> str:
    """Lowercase + collapse non-alphanumerics to single spaces for grouping."""
    return re.sub(r"[^a-z0-9]+", " ", str(title or "").lower()).strip()


def detect_dup_groups(entries: list[dict]) -> list[list[str]]:
    """Groups of >1 idea-status node sharing a normalized title.

    Scoped to the idea pile (where the review-comment harvest creates near-dupes)
    so genuinely distinct ready/done work is never flagged. Returns a list of
    id-lists, each a candidate human merge/supersede set. Never mutates.
    """
    groups: dict[str, list[str]] = {}
    for e in entries:
        if e.get("_status") != "idea":
            continue
        nid = e.get("id")
        if not isinstance(nid, str):
            continue
        key = _normalize_title(e.get("title"))
        if not key:
            continue
        groups.setdefault(key, []).append(nid)
    return [ids for ids in groups.values() if len(ids) > 1]


# ---------------------------------------------------------------------------
# Leg 4: drain stale ideas (propose-only)
# ---------------------------------------------------------------------------

def _parse_ts(value: object) -> Optional[datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


@dataclass
class StaleIdea:
    node_id: str
    age_days: int


def detect_stale_ideas(
    entries: list[dict], staleness_days: int, now: Optional[datetime] = None
) -> list[StaleIdea]:
    """Idea-status nodes STRICTLY older than ``staleness_days`` (no movement).

    Boundary: an idea exactly ``staleness_days`` old is NOT stale (strictly
    older-than, per Failure Modes). Returns candidates for a reversible
    ``defer`` proposal; never mutates.
    """
    if now is None:
        now = datetime.now(timezone.utc)
    out: list[StaleIdea] = []
    for e in entries:
        if e.get("_status") != "idea":
            continue
        nid = e.get("id")
        if not isinstance(nid, str):
            continue
        created = _parse_ts(e.get("created_at"))
        if created is None:
            continue
        age_days = (now - created).days
        if age_days > staleness_days:
            out.append(StaleIdea(node_id=nid, age_days=age_days))
    return out


# ---------------------------------------------------------------------------
# Leg 5: cap Now (propose-only)
# ---------------------------------------------------------------------------

def now_overflow(
    entries: list[dict], cap: int, column_fn
) -> Optional[tuple[int, int]]:
    """Return ``(count, cap)`` when the Now column exceeds ``cap``, else None.

    ``column_fn`` maps an entry to its kanban column (the renderer's
    ``_kanban_column``), injected so this stays decoupled from render.
    """
    count = sum(1 for e in entries if column_fn(e) == "Now")
    return (count, cap) if count > cap else None


# ---------------------------------------------------------------------------
# Leg 7: auto-defer failure-prone nodes (deterministic, --apply only) (#34)
# ---------------------------------------------------------------------------

# Blast-radius guard (Open Question #2): never auto-defer more than this many
# nodes in a single sweep, so a provider-outage mass-failure cannot defer half
# the board. The truncation is always logged by the CLI (no silent cap).
AUTO_DEFER_BLAST_CAP = 10


@dataclass
class FailureDefer:
    node_id: str
    streak: int


def detect_failure_defers(
    entries: list[dict], events, threshold: int
) -> list["FailureDefer"]:
    """Ready nodes whose consecutive-failure streak is ``>= threshold``.

    Mirrors ``detect_temp_leaks`` / ``detect_rescope_fixes``: a pure detector
    that returns candidates (the CLI applies them under one lock). Candidates
    are nodes ``fno backlog next`` would still pick (``_status`` ready, not
    already deferred) - the ones that burn an iteration on every walk. A node
    below threshold, or at exactly ``N-1``, is excluded (Boundaries). The streak
    is derived from the walker's events via ``failure.consecutive_failures``
    (Locked Decision #4); a malformed row is skipped rather than aborting.
    """
    from fno.graph.failure import consecutive_failures

    if threshold < 1:
        return []
    out: list[FailureDefer] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if e.get("_status") != "ready":
            continue
        if e.get("deferred_at"):
            continue
        nid = e.get("id")
        if not isinstance(nid, str):
            continue
        streak = consecutive_failures(nid, events)
        if streak >= threshold:
            out.append(FailureDefer(node_id=nid, streak=streak))
    return out
