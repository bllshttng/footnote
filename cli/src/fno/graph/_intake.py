"""Shared helpers used by cli.py for intake, blocker validation, and project detection.

Extracted from scripts/roadmap-tasks.py to avoid circular imports between
cli.py and the core graph modules.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Literal, Optional, TypedDict, Union

from fno.graph._constants import (
    LEDGER_JSON, PRIORITY_ORDER, is_wellformed_node_id, mint_node_id, _rank_band,
)
from fno import paths as _paths
from fno.graph.depends import (
    _collect_frontmatter_depends,
    _resolve_depends_on,
    _derive_title,
)


# Source-field vocabulary for nodes intaked from a plan. Both spellings
# resolve to the same logical source. "adopt" is the legacy spelling
# from PR #184 and earlier; "intake" is the canonical spelling going
# forward. New writers MUST emit "intake"; readers MUST accept either.
# The migration script (cli/scripts/migrate_source_field.py) rewrites
# existing rows opportunistically, but old graph.json files on backups
# may persist indefinitely so the back-compat is permanent.
INTAKE_SOURCE_VALUES: frozenset[Literal["intake", "adopt"]] = frozenset({"intake", "adopt"})


# -- TypedDicts for intake results --

class _IntakeAlready(TypedDict):
    status: Literal["already"]
    id: str
    title: str


class _IntakeReady(TypedDict):
    status: Literal["ready"]
    node_spec: dict


class _IntakeClaim(TypedDict):
    status: Literal["claim"]
    id: str
    title: str
    claim_source: Literal["cli", "frontmatter"]
    node_spec: dict


_IntakeResult = Union[_IntakeAlready, _IntakeReady, _IntakeClaim]


# -- Graph navigation helpers --

def _find_node(entries: list[dict], node_id: str) -> dict | None:
    # 'ab-' (3) + 8 hex = 11. Anything shorter that starts with 'ab-' is a
    # partial ab-id; route through the fuzzy resolver so callers like
    # `fno backlog done ab-9728` work without typing the full 8 chars.
    # Ambiguity returns None to preserve today's 'not found' contract,
    # but stderr names the candidate IDs so the user can disambiguate
    # rather than seeing a misleading "no such node" from the caller.
    # branch_derived is excluded from the resolved-kinds gate because this
    # helper only takes explicit user input (never a git branch); resolve_id
    # cannot produce branch_derived without a git_branch arg, so the gate
    # is exhaustive for this call shape.
    if node_id.startswith("ab-") and len(node_id) < 11:
        from fno.graph.fuzzy import resolve_id
        match = resolve_id(node_id, entries)
        if match.kind in ("exact", "fuzzy") and match.candidates:
            # candidates[0] is the matched entry; resolve_id already did
            # the iteration so we don't repeat it here.
            return match.candidates[0]
        if match.kind == "ambiguous":
            candidate_ids = ", ".join(
                (e.get("id") or "?") for e in match.candidates
            )
            sys.stderr.write(
                f"[graph] ambiguous prefix '{node_id}' matches: "
                f"{candidate_ids}\n"
            )
        return None
    return next((e for e in entries if e.get("id") == node_id), None)


def _find_dependents(entries: list[dict], node_id: str) -> list[str]:
    return [
        e["id"]
        for e in entries
        if node_id in e.get("blocked_by", [])
    ]


def _would_create_cycle(
    entries: list[dict], node_id: str, proposed_parent_id: str
) -> bool:
    """True iff setting node_id.parent = proposed_parent_id forms a cycle.

    A cycle exists when node_id appears in proposed_parent_id's ancestor
    chain (i.e. node_id is itself an ancestor of the proposed parent).
    Walks proposed_parent_id upward via the parent field; finite because
    cycle-free graphs terminate when parent is None, and we trip the cycle
    bit if we revisit any seen id along the walk.
    """
    if proposed_parent_id == node_id:
        return True
    if not entries:
        return False
    # Build an id -> entry lookup once. Linear scan per step would make
    # the walk O(N x D); the lookup is O(N + D) and matters once the
    # graph grows to thousands of nodes. Guard against non-dict items
    # and missing string ids so a malformed graph.json can't crash the
    # cycle check.
    id_to_entry: dict[str, dict] = {
        e["id"]: e
        for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    seen: set[str] = {node_id}
    current: Optional[str] = proposed_parent_id
    while current:
        if current in seen:
            return True
        seen.add(current)
        ancestor = id_to_entry.get(current)
        if ancestor is None:
            return False
        current = ancestor.get("parent")
    return False


def _would_exceed_epic_depth(
    entries: list[dict], node: dict, parent_node: dict
) -> bool:
    """True iff parenting ``node`` under ``parent_node`` breaks the epic-nesting
    cap (x-6c2b wave 3: mission -> epic -> leaf, two epic levels).

    Fires only when BOTH are epics and either direction would make a third epic
    level: (a) the parent already has an epic ancestor (it is a nested epic, not
    a top-level mission), so ``node`` would be the third level below it; or (b)
    ``node`` already owns an epic subtree, so parenting it under an epic pushes
    its own child epic to the third level. A leaf under any epic, or a childless
    epic under a mission, is allowed. Cycle-safe (seen set / descendants_of).
    """
    if node.get("type") != "epic" or parent_node.get("type") != "epic":
        return False
    id_to_entry = {
        e["id"]: e
        for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    # (a) walk UP from the parent: any epic ancestor means the parent is nested.
    seen: set[str] = set()
    current = parent_node.get("parent")
    while current and current not in seen:
        seen.add(current)
        ancestor = id_to_entry.get(current)
        if ancestor is None:
            break
        if ancestor.get("type") == "epic":
            return True
        current = ancestor.get("parent")
    # (b) walk DOWN from node: an epic descendant means node is itself a
    # mission, so nesting it under another epic exceeds the cap.
    nid = node.get("id")
    if nid:
        for desc_id in descendants_of(entries, nid):
            desc = id_to_entry.get(desc_id)
            if desc and desc.get("type") == "epic":
                return True
    return False


def descendants_of(entries: list[dict], parent_id: str) -> set[str]:
    """Return the set of node IDs that are transitive children of ``parent_id``.

    Walks the ``parent`` field downward (epic -> group children -> any
    deeper children) via a children-by-parent index, returning every node
    reachable below ``parent_id``. The parent itself is never included.

    Powers the ``--parent <epic-id>`` epic-scope filter on ``fno backlog
    next``/``ready`` (C2, ab-facfaade): candidates are restricted to this
    set so a walk drains one epic's subtree feature-by-feature.

    Cycle-safe: a ``seen`` set bounds the BFS so a malformed graph with a
    ``parent`` cycle terminates instead of looping. Non-dict items and
    entries without a string ``id`` are skipped, mirroring the defensive
    posture of ``_would_create_cycle`` and ``make_selection_sort_key``.
    An unknown ``parent_id`` (or one with no children) returns an empty
    set; callers validate node existence separately before filtering.
    """
    children_by_parent: dict[str, list[str]] = {}
    for e in entries:
        if not isinstance(e, dict):
            continue
        node_id = e.get("id")
        pid = e.get("parent")
        # Both keys must be strings: a corrupted row with a dict/list parent
        # would otherwise raise TypeError on the dict insert below.
        if isinstance(node_id, str) and isinstance(pid, str):
            children_by_parent.setdefault(pid, []).append(node_id)

    result: set[str] = set()
    frontier = list(children_by_parent.get(parent_id, []))
    while frontier:
        current = frontier.pop()
        # Never include the scoped parent itself, even if a malformed cycle
        # points a descendant's child chain back to it (contract: parent is
        # excluded). The `in result` guard bounds the BFS against cycles.
        if current in result or current == parent_id:
            continue
        result.add(current)
        frontier.extend(children_by_parent.get(current, []))
    return result


def _graph_sort_key_fn(e: dict) -> tuple:
    return (PRIORITY_ORDER.get(e.get("priority", "p2"), 2), e.get("created_at", ""))


def make_selection_sort_key(entries: list[dict], orphans: Optional[frozenset[str]] = None):
    """Build the rank-then-epics-first selection sort key (Locked Decision 7, C3).

    Returns a key function for sorting *ready candidates* by selection
    precedence: curated ``rank`` first, then epics-first, then flat priority.
    The key prepends the SAME ``_rank_band`` term the board lane key uses
    (``render._lane_sort_key``), so a ``fno backlog rank --top`` node is
    *worked* next, not merely floated on the board - board order and work
    order share one rank definition and cannot drift (Locked
    Decision 4). A ranked node (band 0, ascending rank) outranks every
    unranked node, so an explicit rank overrides the epics-first heuristic;
    with no ranks set every node shares the ``(1, 0.0)`` band and ordering is
    byte-for-byte today's epics-first behavior. A node is an "epic child"
    when its ``parent`` resolves to another node in ``entries``; such
    children always outrank loose nodes regardless of raw priority, so a
    walk stays focused on one epic before starting loose work. Among epic
    children the order is: in-progress epics first (an epic with a done or
    claimed child), then higher-priority epics, then the epic's own
    ``created_at`` (keeps one epic's children grouped), then the child's
    own priority and ``created_at``. Loose nodes fall back to flat
    priority then ``created_at`` (matching ``_graph_sort_key_fn``).

    The key is precomputed against ``entries`` once so sorting stays O(N
    log N): epic lookup, child grouping, and in-progress detection are all
    table lookups. A ``parent`` that names a missing node is treated as a
    loose node (never crashes on a malformed graph).
    """
    id_to_entry: dict[str, dict] = {
        e["id"]: e
        for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    # Board == work order: `next` must demote orphans exactly where the board
    # does, or the board shows one order and the walker works another. Computed
    # here (not passed by every caller) so no call site can forget it; fails
    # open to "no orphans", which reproduces the pre-rollup ordering.
    if orphans is None:
        try:
            from fno.graph.rollup import orphan_ids

            orphans = orphan_ids(entries)
        except Exception:  # noqa: BLE001 - ordering signal; never break selection
            orphans = frozenset()
    children_by_parent: dict[str, list[dict]] = {}
    for e in entries:
        if not isinstance(e, dict):
            continue
        pid = e.get("parent")
        if pid:
            children_by_parent.setdefault(pid, []).append(e)
    # An epic is "in progress" when any of its children is done or claimed.
    epic_in_progress: dict[str, bool] = {
        pid: any(
            kid.get("status") == "done" or kid.get("session_id")
            for kid in kids
        )
        for pid, kids in children_by_parent.items()
    }

    def _prio(e: dict) -> int:
        return PRIORITY_ORDER.get(e.get("priority", "p2"), 2)

    def key(node: dict) -> tuple:
        # Curated rank leads: the SAME `_rank_band` the board uses,
        # prepended so a `rank --top` node (band 0, ascending rank) is selected
        # ahead of ALL unranked nodes (band 1) - including in-progress epic
        # children, so an explicit rank overrides the epics-first heuristic
        # (Locked Decision 1). Unranked nodes all share the `(1, 0.0)` band, so
        # the existing epics-first key below decides their order byte-for-byte.
        band = _rank_band(node)
        child_prio = _prio(node)
        child_orphan = node.get("id") in orphans
        child_created = node.get("created_at", "") or ""
        pid = node.get("parent")
        epic = id_to_entry.get(pid) if pid else None
        if epic is not None:
            in_progress_rank = 0 if (pid and epic_in_progress.get(pid)) else 1
            return (
                band,                    # curated rank band (ranked first)
                0,                       # epic-children tier (before loose)
                in_progress_rank,        # in-progress epics first
                _prio(epic),             # highest-priority epic first
                epic.get("created_at", "") or "",  # group one epic together
                child_prio,
                child_orphan,        # in-band: after priority, before created_at
                child_created,
            )
        # Loose node: tier 1. Middle fields mirror the child fields so the
        # tuple stays comparable; tier already separates loose from epic
        # children, so loose nodes only ever compare among themselves and
        # resolve on flat (priority, created_at).
        # Decision fields lead: priority, then orphan-last, then created_at.
        # The trailing pair only pads the tuple to the epic branch's arity;
        # tier (index 1) already separates the two, so it is never compared.
        return (
            band, 1, 0, child_prio, child_orphan, child_created,
            child_prio, child_created,
        )

    return key


# -- Project detection --

def _git_repo_root() -> str | None:
    # Canonical (main) working tree via the shared resolver: first non-`bare`
    # `git worktree list` record that is a real working tree, robust across
    # normal / bare / separate-git-dir layouts (skips a bare repo and a
    # separate-git-dir gitdir mis-report rather than recording either as the
    # backlog cwd). See paths.resolve_canonical_worktree (ab-91a004af).
    canonical = _paths.resolve_canonical_worktree()
    if canonical is not None:
        return os.path.normpath(str(canonical))
    # Fallback (helper found no usable working tree, e.g. bare-only or
    # separate-git-dir with no linked checkout): the current checkout via
    # --show-toplevel - a real working tree, never a git dir - else None.
    try:
        top = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"],
            stderr=subprocess.DEVNULL, text=True, encoding="utf-8",
        ).strip()
        return os.path.normpath(top) if top else None
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def repo_root() -> str:
    """Absolute path of the canonical (main) checkout.

    Resolves through ``_git_repo_root()`` (the main-worktree path from
    ``git worktree list --porcelain``), so it returns the *main* worktree even
    when invoked from a linked worktree; falls back to ``os.getcwd()`` outside
    a git repo.

    This is the path durable artifacts (backlog nodes, think docs,
    blueprints) must record as ``cwd``: they outlive the worktree that
    created them, so a worktree top-level path (``--show-toplevel``) would
    go stale once that worktree is archived. Contrast ephemeral session
    state (``target-state.md``), which is deliberately bound to its worktree.
    """
    return _git_repo_root() or os.getcwd()


def _settings_candidate_paths() -> list[Path]:
    """Settings files to consult for project<->path config, nearest first.

    Project-local (cwd-relative, then the file ``load_settings()`` actually
    used) followed by GLOBAL ``~/.fno/settings.yaml``. The global file is
    essential: the ``work.workspaces`` project->path map lives only there, and
    inside a repo ``config_file()`` resolves to the project-local settings, so
    without the global entry the map is never consulted and project detection
    returns None for every node filed from inside a project (ab-95e8efec).

    De-duplicated by normalized path so an outside-a-repo run (where
    ``config_file()`` is already the global file) does not read + warn on the
    same file twice. Shared by :func:`detect_project_from_settings` and
    :func:`_list_known_projects` so the two readers cannot drift apart again.
    """
    # The canonical "where is the global settings file" resolver. Honors the
    # $FNO_GLOBAL_SETTINGS_PATH redirect (config_file() / state_dir() do NOT
    # follow it), so a redirected global work-map is still consulted (codex P2
    # on PR #419).
    # Function-local: config_io imports pydantic/yaml at module load; a top-level
    # import here would pull them into every graph-module import (bare-python
    # smoke consumers of the graph package have neither). config_io (a leaf) not
    # fno.config, so no config<->graph cycle.
    from fno.config_io import _global_settings_path, config_read_candidates

    out: list[Path] = []
    seen: set[str] = set()
    # config.toml-first: each settings.yaml location also yields its config.toml
    # sibling as a higher-priority candidate (flat hard cut, x-8526).
    for path in config_read_candidates([
        Path(".fno/settings.yaml"),
        _paths.config_file(),
        _global_settings_path(),
    ]):
        # abspath (not normpath) so the cwd-relative candidate and an absolute
        # config_file() pointing at the SAME project-local file dedup to one
        # read (gemini MEDIUM on PR #419); normpath leaves one relative + one
        # absolute, which never match.
        key = os.path.abspath(os.path.expanduser(str(path)))
        if key in seen:
            continue
        seen.add(key)
        out.append(path)
    return out


def detect_project_from_settings(cwd_path: str | None = None) -> str | None:
    """Auto-detect project name from settings.yaml work config.

    Walks ``.fno/settings.yaml`` (project-local) then
    ``~/.fno/settings.yaml`` (global). Returns the first project whose
    ``path`` matches ``cwd_path`` (after expanduser + normpath). Silent on
    failure: missing files, parse errors, or no match all return None so
    auto-detection never breaks ``backlog add`` for users without settings.

    Schema accepted:

    Multi-workspace (canonical, current settings.yaml shape)::

        work:
          workspaces:
            <ws-name>:
              projects:
                - name: my-proj
                  path: ~/code/my-proj

    Legacy flat (older settings.yaml shape, kept for back-compat)::

        work:
          projects:
            my-proj:
              path: ~/code/my-proj
    """
    # abspath (not just normpath) so a relative cwd like "." resolves to the
    # full absolute path before comparison. settings.yaml `path:` entries are
    # stored as ~ / absolute, so a relative target would never match.
    target = os.path.abspath(os.path.expanduser(cwd_path)) if cwd_path else os.getcwd()

    # Function-local: keep graph-module load free of config_io's pydantic/yaml.
    from fno.config_io import read_config_flat

    for path in _settings_candidate_paths():
        if not path.exists():
            continue
        # read_config_flat parses config.toml (or a legacy settings.yaml) and
        # returns the FLAT dict; work is top-level. A missing/unparseable file
        # contributes nothing (best-effort, per the silent-failure contract).
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
                for p in projects:
                    if not isinstance(p, dict):
                        continue
                    raw_path = p.get("path")
                    name = p.get("name")
                    if not raw_path or not name:
                        continue
                    proj_path = os.path.normpath(os.path.expanduser(str(raw_path)))
                    if proj_path == target:
                        return str(name)

        flat_projects = work.get("projects")
        if isinstance(flat_projects, dict):
            for name, cfg in flat_projects.items():
                if not isinstance(cfg, dict):
                    continue
                raw_path = cfg.get("path")
                if not raw_path:
                    continue
                proj_path = os.path.normpath(os.path.expanduser(str(raw_path)))
                if proj_path == target:
                    return str(name)

    return None


def project_root_from_settings(project: str | None) -> str | None:
    """Inverse of detect_project_from_settings: project name -> work-map path.

    Walks the same candidate settings files (via ``_settings_candidate_paths``)
    in the same order as the forward direction so the two cannot drift apart.
    Returns ``os.path.abspath(os.path.expanduser(path))`` for the first match
    found, or ``None`` when:

    - ``project`` is falsy (None or empty string)
    - no settings file maps that project name
    - all settings files are missing, unreadable, or malformed

    Pure map lookup: no stat(), no git calls, no existence check. A mapped-but-
    missing path must fail loudly downstream, not silently fall back here.

    Schema accepted (same two variants as ``detect_project_from_settings``):

    Multi-workspace::

        work:
          workspaces:
            <ws-name>:
              projects:
                - name: my-proj
                  path: ~/code/my-proj

    Legacy flat::

        work:
          projects:
            my-proj:
              path: ~/code/my-proj
    """
    if not project:
        return None

    # Function-local: keep graph-module load free of config_io's pydantic/yaml.
    from fno.config_io import read_config_flat

    for path in _settings_candidate_paths():
        if not path.exists():
            continue
        # read_config_flat parses config.toml (or a legacy settings.yaml) and
        # returns the FLAT dict; work is top-level. A missing/unparseable file
        # contributes nothing (best-effort, same as the forward reader).
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
                for p in projects:
                    if not isinstance(p, dict):
                        continue
                    name = p.get("name")
                    raw_path = p.get("path")
                    if not name or not raw_path:
                        continue
                    if name == project:
                        return os.path.abspath(os.path.expanduser(str(raw_path)))

        flat_projects = work.get("projects")
        if isinstance(flat_projects, dict):
            for name, cfg in flat_projects.items():
                if not isinstance(cfg, dict):
                    continue
                raw_path = cfg.get("path")
                if not raw_path:
                    continue
                if name == project:
                    return os.path.abspath(os.path.expanduser(str(raw_path)))

    return None


def _read_plan_frontmatter(plan_path: str) -> dict:
    """Parse YAML frontmatter from a single-file plan.

    Returns the parsed frontmatter as a dict. Returns ``{}`` on every
    failure mode (missing file, malformed YAML, no frontmatter, missing
    PyYAML) so the caller can use the result without try/except. Malformed
    YAML emits one stderr warning so the user knows their file is broken
    instead of silently routing on stale data; every other failure is
    silent because the caller's chain already handles "no frontmatter
    declared" as a valid state.
    """
    target = Path(plan_path)
    if not target.exists() or not target.is_file():
        return {}

    try:
        text = target.read_text()
    except (OSError, UnicodeDecodeError):
        # UnicodeDecodeError is a ValueError, so letting it escape turns a
        # caller's designed "no frontmatter" branch into an error exit.
        return {}

    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return {}

    end_idx: int | None = None
    for i in range(1, len(lines)):
        if lines[i].strip() == "---":
            end_idx = i
            break
    if end_idx is None:
        return {}

    fm_text = "\n".join(lines[1:end_idx])

    try:
        import yaml
    except ImportError:
        sys.stderr.write(
            "warning: PyYAML missing - plan-frontmatter project resolution disabled\n"
        )
        return {}

    try:
        data = yaml.safe_load(fm_text)
    except yaml.YAMLError as e:
        sys.stderr.write(f"warning: could not parse frontmatter in {target}: {e}\n")
        return {}

    if not isinstance(data, dict):
        return {}
    return data


def _list_known_projects() -> set[str]:
    """Return all project names declared across project-local and global settings.yaml.

    Mirrors the schema walk in ``detect_project_from_settings``. Returns an
    empty set if no settings file is readable or PyYAML is unavailable.
    Used by the post-resolution sanity check that warns when an intake
    routes to a project the orchestrator has never heard of.
    """

    known: set[str] = set()
    # Function-local: keep graph-module load free of config_io's pydantic/yaml.
    from fno.config_io import read_config_flat

    for path in _settings_candidate_paths():
        if not path.exists():
            continue
        # config.toml (or legacy settings.yaml) -> flat dict; work is top-level.
        work = read_config_flat(path).get("work")
        if not isinstance(work, dict):
            continue

        workspaces = work.get("workspaces")
        if isinstance(workspaces, dict):
            for ws in workspaces.values():
                if not isinstance(ws, dict):
                    continue
                projects = ws.get("projects")
                if isinstance(projects, list):
                    for proj in projects:
                        if isinstance(proj, dict) and isinstance(proj.get("name"), str):
                            known.add(proj["name"])

        flat_projects = work.get("projects")
        if isinstance(flat_projects, dict):
            for name in flat_projects.keys():
                if isinstance(name, str):
                    known.add(name)

    return known


def _warn_unknown_project(project: str | None, known: set[str] | None = None) -> None:
    """Emit one stderr line if ``project`` is not in any known settings workspace.

    Silent when ``project`` is falsy or when no settings file declares any
    workspace at all (a brand-new install has nothing to compare against).
    Callers in tight loops (e.g. multi-plan intake) can pass a pre-computed
    ``known`` set to avoid re-reading settings.yaml per iteration.
    """
    if not project:
        return
    if known is None:
        known = _list_known_projects()
    if not known:
        return
    if project not in known:
        sys.stderr.write(
            f"warning: project '{project}' is not declared in any settings.yaml "
            f"workspace; work-routing may not happen\n"
        )


def detect_project(entries: list[dict]) -> str | None:
    norm_root = os.path.normpath(repo_root())
    root_with_sep = norm_root.rstrip(os.sep) + os.sep
    fallback_project: str | None = None
    for e in entries:
        entry_cwd = e.get("cwd")
        if not entry_cwd:
            continue
        # expanduser BEFORE normpath so historical entries stored with
        # tilde-form paths (e.g. "~/code/me/chingu") match the absolute
        # repo_root. The writer side (cmd_add/cmd_idea) stores absolute
        # paths since PR #167, but pre-#167 entries and any direct edits
        # use ~. Without expanduser, the comparison silently never matches
        # and detect_project falls through to the global-scope fallback.
        norm_cwd = os.path.normpath(os.path.expanduser(str(entry_cwd)))
        if norm_cwd == norm_root:
            return e.get("project")
        if fallback_project is None and norm_cwd.startswith(root_with_sep):
            fallback_project = e.get("project")
    return fallback_project


def filter_by_project(entries: list[dict], project: str | None, show_all: bool) -> list[dict]:
    if project:
        return [e for e in entries if e.get("project") == project]
    if show_all:
        return entries
    detected = detect_project(entries)
    if detected:
        return [e for e in entries if e.get("project") == detected]
    return entries


# -- Blocker helpers --

def _parse_blocker_list(values: list[str] | None) -> list[str]:
    if not values:
        return []
    out: list[str] = []
    for v in values:
        if v is None:
            continue
        for token in v.split(","):
            t = token.strip()
            if t:
                out.append(t)
    return out


def _validate_blocker_ids(blockers: list[str], entries: list[dict], task_id: str) -> None:
    import typer
    id_to_entry = {e.get("id"): e for e in entries if isinstance(e.get("id"), str)}
    for bid in blockers:
        if bid not in id_to_entry:
            typer.echo(f"Error: unknown blocker id '{bid}'", err=True)
            raise typer.Exit(code=2)
        if bid == task_id:
            typer.echo(f"Error: node cannot block itself ({task_id})", err=True)
            raise typer.Exit(code=2)
        visited: set[str] = set()
        stack = [bid]
        while stack:
            curr = stack.pop()
            if curr == task_id:
                typer.echo(
                    f"Error: cycle detected - {bid} (transitively) depends on {task_id}",
                    err=True,
                )
                raise typer.Exit(code=2)
            if curr in visited:
                continue
            visited.add(curr)
            curr_entry = id_to_entry.get(curr)
            if curr_entry:
                stack.extend(curr_entry.get("blocked_by", []))


def _validate_cli_deps(cli_deps: list[str], entries: list[dict]) -> None:
    import typer
    if not cli_deps:
        return
    id_set = {e.get("id") for e in entries if isinstance(e.get("id"), str)}
    missing = [d for d in cli_deps if d not in id_set]
    if missing:
        for d in missing:
            typer.echo(f"Error: dependency not found in graph: {d}", err=True)
        raise typer.Exit(code=1)


# -- Ledger lookup --

def _lookup_ledger_entry(plan_path: str) -> dict | None:
    if not LEDGER_JSON.exists():
        return None
    try:
        data = json.loads(LEDGER_JSON.read_text())
    except (json.JSONDecodeError, OSError) as e:
        print(
            f"Warning: could not read {LEDGER_JSON}: {e} - "
            "intake will fall back to filename-derived title/points",
            file=sys.stderr,
        )
        return None
    target = os.path.normpath(plan_path)
    for entry in data.get("entries", []):
        ep = entry.get("plan_path")
        if ep and os.path.normpath(ep) == target:
            return entry
    return None


def _match_plan_in_graph(
    entries: list[dict], plan_path: str, roadmap_id: str | None,
) -> dict | None:
    target = os.path.normpath(plan_path)
    for e in entries:
        ep = e.get("plan_path")
        if ep and os.path.normpath(ep) == target and e.get("roadmap_id") == roadmap_id:
            return e
    return None


def _resolve_claim(
    cli_claim: str | None,
    plan_path: str,
    entries: list[dict],
) -> tuple[dict | None, Optional[Literal["cli", "frontmatter"]]]:
    """Resolve a plan's claim to an existing node.

    Returns ``(node, source)`` where ``source`` is ``"cli"`` if the CLI flag
    supplied the id, ``"frontmatter"`` if the plan's frontmatter did, or
    ``None`` when neither layer declared a claim.

    Precedence: CLI flag wins over frontmatter when both are present. When
    the CLI flag is set, the frontmatter is not read - matches operator
    workflows where the plan may not have parseable frontmatter at all.

    Raises:
        ValueError: when the supplied id is malformed or names a node that
            does not exist on the graph. Malformed frontmatter values are
            silently ignored (treated as no claim) so an unrelated typo in
            an existing plan never blocks intake; the CLI flag is the
            authoritative path and is strict.
    """
    fm_claim: str | None = None
    if cli_claim is None:
        fm = _read_plan_frontmatter(plan_path)
        fm_claim_raw = fm.get("claims")
        if isinstance(fm_claim_raw, str):
            stripped = fm_claim_raw.strip()
            if is_wellformed_node_id(stripped):
                fm_claim = stripped
            # Malformed frontmatter -> treat as "no claim", do not raise.

    raw = (cli_claim or fm_claim or "").strip()
    if not raw:
        return (None, None)

    if not is_wellformed_node_id(raw):
        # Only reachable when the CLI flag carried a malformed value -
        # frontmatter never gets here because we already filtered it above.
        raise ValueError(
            f"invalid claims value: {raw!r} (expected a <prefix>-<4..8 hex> node id)"
        )

    for e in entries:
        if e.get("id") == raw:
            source: Literal["cli", "frontmatter"] = (
                "cli" if cli_claim else "frontmatter"
            )
            return (e, source)

    raise ValueError(
        f"claims target {raw} not found on graph - "
        f"check the ID or remove the claim"
    )


def _warn_similar_idea_titles(
    new_title: str,
    new_id: str,
    entries: list[dict],
    threshold: float = 0.7,
) -> None:
    """Emit a stderr warning naming the closest idea-state node by title.

    Safety net for the third claims layer: a plan that did not declare a
    claim but whose title strongly resembles an existing idea-state node is
    very likely meant to claim it. We do not auto-merge - we just warn so
    the author can re-run with ``--claims <id>``.

    The scan is bounded by ``threshold`` (default 0.7 via
    ``difflib.SequenceMatcher``). Only ``status: idea`` nodes are
    considered; non-idea states are skipped because claiming them is
    refused upstream anyway. ``new_id`` is excluded so a node never
    triggers a warning about itself.
    """
    from difflib import SequenceMatcher
    new_title_norm = new_title.strip().lower()
    if not new_title_norm:
        return
    matches: list[tuple[float, str, str]] = []
    for e in entries:
        if e.get("id") == new_id:
            continue
        if e.get("status") != "idea":
            continue
        title = (e.get("title") or "").strip().lower()
        if not title:
            continue
        ratio = SequenceMatcher(None, title, new_title_norm).ratio()
        if ratio >= threshold:
            matches.append((ratio, e.get("id") or "", e.get("title") or ""))
    if not matches:
        return
    matches.sort(reverse=True)
    top_ratio, top_id, top_title = matches[0]
    sys.stderr.write(
        f'warning: idea-state node {top_id} has a similar title '
        f'("{top_title}", ratio={top_ratio:.2f}); if this plan implements '
        f'it, rerun with --claims {top_id} to claim the existing node\n'
    )


# -- Intake core --

def _prepare_intake(
    plan_path: str,
    entries: list[dict],
    *,
    roadmap_id: str | None,
    cli_title: str | None,
    cli_priority: str | None,
    cli_deps: list[str],
    cli_points: int | None,
    cli_project: str | None = None,
    cli_claim: str | None = None,
) -> _IntakeResult:
    # Claim resolution runs FIRST so a claim on an existing idea node beats
    # the plan_path-equality match. _resolve_claim raises ValueError on bad
    # input; the caller surfaces those as non-zero exits via Typer.
    claim_node, claim_source = _resolve_claim(cli_claim, plan_path, entries)
    if claim_node is not None:
        node_status = claim_node.get("status")
        if node_status not in ("idea", None):
            raise ValueError(
                f'node {claim_node.get("id")} is in state {node_status!r}; '
                f"refuse to claim a non-idea node"
            )

    existing = _match_plan_in_graph(entries, plan_path, roadmap_id)
    fm_raw, plan_dir = _collect_frontmatter_depends(plan_path)
    resolved_fm, unresolved_fm = _resolve_depends_on(fm_raw, entries, plan_dir)
    for u in unresolved_fm:
        print(
            f"Warning: depends_on '{u}' for {plan_path} not found on graph - skipping edge",
            file=sys.stderr,
        )

    all_deps = list(dict.fromkeys([*cli_deps, *resolved_fm]))

    ledger_entry = _lookup_ledger_entry(plan_path)
    p = Path(plan_path)
    if p.is_file():
        title = _derive_title(p, cli_title or ((ledger_entry or {}).get("title")))
    else:
        title = (
            cli_title
            or (ledger_entry or {}).get("title")
            or os.path.basename(os.path.normpath(plan_path))
        )
    points = cli_points if cli_points is not None else (ledger_entry or {}).get("points")
    priority = cli_priority or "p2"

    if existing:
        # When a claim is set AND the plan_path already matches a different
        # node, surface the conflict instead of silently no-op'ing. This is
        # exactly the "target created ab-XYZ before --claims existed" repair
        # path - the user needs to drop ab-XYZ (or supersede it) before the
        # claim can land on the original idea node.
        if claim_node is not None and existing.get("id") != claim_node.get("id"):
            raise ValueError(
                f'plan_path already intaked as {existing["id"]}, but --claims '
                f'names {claim_node["id"]}. Remove or supersede {existing["id"]} '
                f'before claiming, e.g.:\n'
                f'  fno backlog supersede {claim_node["id"]} '
                f'--replaces {existing["id"]} --reason "..."'
            )
        return {
            "status": "already",
            "id": existing["id"],
            "title": title,
        }

    node_spec = {
        "plan_path": plan_path,
        "roadmap_id": roadmap_id,
        "title": title,
        "priority": priority,
        "deps": all_deps,
        "points": points,
        "cli_project": cli_project,
    }

    if claim_node is not None:
        # claim_source is non-None whenever claim_node is non-None, but type
        # narrowers can't see that without an assert.
        assert claim_source is not None
        return {
            "status": "claim",
            "id": claim_node["id"],
            "title": title,
            "claim_source": claim_source,
            "node_spec": node_spec,
        }

    return {
        "status": "ready",
        "node_spec": node_spec,
    }


def resolve_git_roots() -> tuple[str | None, str | None]:
    """Return ``(project_name, canonical_root)`` for the current git context.

    - ``project_name``: basename of the main repo root (via
      ``_git_repo_root()``). Same across all worktrees of the same repo,
      so ``fno backlog ready --project <name>`` works regardless of which
      worktree created the node.
    - ``canonical_root``: absolute path of the main checkout (via
      ``_git_repo_root()`` / the first ``git worktree list`` entry). For the
      canonical/main checkout this resolves to the repository root; for linked
      worktrees it resolves back to the main checkout rather than the
      (ephemeral) worktree path.

    Durable artifacts record ``canonical_root`` as ``cwd`` so the reference
    survives the worktree being archived - see ``repo_root()``. An explicit
    ``--cwd`` always wins at the call sites.

    Returns ``(None, None)`` when not in a git repo. Shared by
    ``resolve_node_project_and_cwd`` (plan-path intake) and ``cmd_new``
    (graph node creation without a plan).
    """
    main_root = _git_repo_root()
    derived_name: str | None = None
    if main_root:
        basename = os.path.basename(main_root)
        if basename and basename != os.sep:
            derived_name = basename
    return (derived_name, main_root)


def resolve_node_project_and_cwd(
    plan_path: str,
    cli_project: str | None,
    entries: list[dict],
) -> tuple[str | None, str, dict]:
    """Resolve (project, cwd, parsed_frontmatter) for a node from a plan path.

    Shared by ``_build_intake_node`` (fresh intake) and the claim-backfill
    path (where the node already exists from ``fno backlog new`` but has
    null project/cwd). Returns the parsed frontmatter as the third tuple
    element so callers that need additional fields (mission_*, etc.)
    don't have to re-read the file from disk.

    Resolution chain:

    - ``project`` = ``cli_project`` > frontmatter ``project`` > settings.yaml
      project matching the canonical root > basename of main git root
      (NOT worktree basename - see ``resolve_git_roots`` rationale)
    - ``cwd`` = frontmatter ``cwd`` (expanduser'd) > canonical main
      checkout > ``os.getcwd()``

    Worktree-aware for the project NAME (canonical basename, shared across
    worktrees) but the recorded ``cwd`` is the canonical main checkout, not
    the creating worktree: a backlog node outlives the worktree it was
    filed from, so a worktree path would dangle once archived. See
    ``repo_root()`` / ``resolve_git_roots()``.

    Non-string frontmatter values trigger a stderr warning and are
    treated as null, same as the original code path.
    """
    derived_name, canonical_root = resolve_git_roots()
    canonical_root = canonical_root or os.getcwd()

    fm = _read_plan_frontmatter(plan_path)

    fm_project_raw = fm.get("project")
    fm_project = (
        fm_project_raw
        if isinstance(fm_project_raw, str) and fm_project_raw.strip()
        else None
    )
    if fm_project_raw is not None and fm_project is None:
        sys.stderr.write(
            f"warning: rejecting non-string project value in plan frontmatter: "
            f"{fm_project_raw!r}\n"
        )
    project = (
        cli_project
        or fm_project
        or detect_project(entries)
        or derived_name
    )

    fm_cwd_raw = fm.get("cwd")
    fm_cwd = (
        os.path.expanduser(fm_cwd_raw)
        if isinstance(fm_cwd_raw, str) and fm_cwd_raw.strip()
        else None
    )
    if fm_cwd_raw is not None and fm_cwd is None:
        sys.stderr.write(
            f"warning: rejecting non-string cwd value in plan frontmatter: "
            f"{fm_cwd_raw!r}\n"
        )
    # Derive cwd from the work-map when the project came from an explicit
    # source (CLI flag or frontmatter) and no explicit cwd was provided.
    # Auto-detected and basename-derived projects are not eligible - the cwd
    # was the input to detection so they are already consistent.
    explicit = bool(cli_project or fm_project)
    if fm_cwd is None and explicit and project:
        workmap_root = project_root_from_settings(project)
        node_cwd = workmap_root or canonical_root
    else:
        node_cwd = fm_cwd or canonical_root

    return (project, node_cwd, fm)


def normalize_size(value: object) -> Optional[str]:
    """Coerce a plan-frontmatter size to the canonical S|M|L, else None.

    Sizes are declared lowercase or uppercase in plan frontmatter; the graph
    stores them uppercase. Anything outside {S,M,L} is dropped (never stored).
    """
    if not value:
        return None
    s = str(value).strip().upper()
    return s if s in {"S", "M", "L"} else None


def _build_intake_node(spec: dict, entries: list[dict]) -> dict:
    from datetime import datetime, timezone

    project, node_cwd, fm = resolve_node_project_and_cwd(
        spec["plan_path"], spec.get("cli_project"), entries
    )

    # Preserve mission context fields from plan frontmatter so megawalk can
    # propagate them as TARGET_MISSION_* env vars (Task 3.1). Nullable strings
    # for mission_id / mission_slug / mission_from_msg_id; nullable int for
    # mission_wave. Absence means the plan is not part of a megatron mission.
    mission_id = fm.get("mission_id") or None
    mission_wave_raw = fm.get("mission_wave")
    if mission_wave_raw is not None:
        try:
            mission_wave: Optional[int] = int(mission_wave_raw)
        except (TypeError, ValueError):
            sys.stderr.write(
                f"warning: non-integer mission_wave {mission_wave_raw!r} in "
                f"{spec['plan_path']} frontmatter; storing as null\n"
            )
            mission_wave = None
    else:
        mission_wave = None
    mission_slug: Optional[str] = fm.get("mission_slug") or None
    mission_from_msg_id: Optional[str] = fm.get("mission_from_msg_id") or None

    return {
        "id": mint_node_id({e.get("id") for e in entries if e.get("id")}),
        "parent": None,
        "title": spec["title"],
        "type": "feature",
        "project": project,
        "cwd": node_cwd,
        "priority": spec["priority"],
        "domain": "code",
        "blocked_by": spec["deps"],
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "has_brief": False,
        "roadmap_id": spec["roadmap_id"],
        "vision_path": None,
        "details": None,
        # Size flows doc->graph at intake: the plan frontmatter is where size is
        # declared, and the graph never captured it before (7.3% fill). null when
        # the plan omits it or declares a non-S/M/L value.
        "size": normalize_size(fm.get("size")),
        "batch": None,
        "cost_usd": None,
        "cost_sessions": [],
        "plan_path": spec["plan_path"],
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "artifact_url": None,
        "completion_note": None,
        "points": spec.get("points"),
        "source": "intake",
        "created_at": datetime.now(timezone.utc).isoformat(),
        # Mission context: only present when the plan was spawned by megatron.
        # Preserved verbatim so megawalk.extract_mission_env can read them.
        "mission_id": mission_id,
        "mission_wave": mission_wave,
        "mission_slug": mission_slug,
        "mission_from_msg_id": mission_from_msg_id,
    }


def _collect_intake_paths(args) -> list[str]:
    paths: list[str] = []
    src = getattr(args, "from_list", None)
    if src:
        if src == "-":
            raw = sys.stdin.read()
        else:
            try:
                raw = Path(src).read_text()
            except OSError as e:
                print(f"Error: --from {src}: {e}", file=sys.stderr)
                sys.exit(1)
        for line in raw.splitlines():
            s = line.strip()
            if not s or s.startswith("#"):
                continue
            paths.append(s)
    for p in getattr(args, "plan_paths", None) or []:
        if "," in p and not os.path.exists(p):
            for part in p.split(","):
                part = part.strip()
                if part:
                    paths.append(part)
        else:
            paths.append(p)
    return paths
