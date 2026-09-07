"""Active-backlog drain-target resolution.

Resolves which ACTIVE MISSIONS the always-on backlog dispatcher daemon should
drain (x-a4dc K2): one target per epic with ``mission_active=true``, from the
graph plus the workspace project->path map, gated by ``config.active_backlog``.
The daemon is a per-user global process with no inherent project, so it shells
``fno config active-backlog --json`` once on entering Serving to learn its drain
targets (mission epic + cwd + cadence + failure limit) - keeping all config logic
in Python, the single source of truth, exactly like the rest of the daemon's
config-ish reads. It drains each mission by shelling K1's converge core
(``advance --epic``); the legacy per-project interval drain is deleted.

Pure + best-effort: a malformed settings file or graph yields no targets rather
than raising, so the daemon never crashes on an operator config typo.
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Optional

# The wake nudge sentinel filename under the state dir. The daemon watches this
# file's mtime; a backlog mutation / advance dispatch touches it to wake the
# drain loop sooner than the poll floor. The Rust watcher resolves the same file
# under $HOME/.fno (the default state dir); a non-default state_dir only loses
# the latency optimization, never correctness (the poll floor is the guarantee).
NUDGE_SENTINEL_NAME = ".active-backlog-nudge"


def nudge_sentinel_path() -> Path:
    """Resolve the nudge sentinel path via the configured state dir."""
    from fno.paths import state_dir

    return state_dir() / NUDGE_SENTINEL_NAME


def touch_nudge() -> None:
    """Best-effort touch of the wake nudge sentinel; never raises.

    Called from `locked_mutate_graph` (after a board render) and from
    `fno backlog advance`. A failed write is harmless: the daemon's poll floor
    drains the new work within one interval regardless (Locked Decision 7 - the
    poll floor is the correctness guarantee, the nudge is a latency optimization).
    """
    try:
        p = nudge_sentinel_path()
        p.parent.mkdir(parents=True, exist_ok=True)
        p.touch()
    except Exception:
        pass


@dataclass(frozen=True)
class DrainTarget:
    """One active mission the daemon should continuously drain (x-a4dc K2).

    ``mission`` is the epic id the daemon converges (``advance --epic``); ``project``/
    ``cwd`` are the epic's own project, rooting the loop's journal + close/defer reads
    (all node-global operations - a mission fans out across projects at dispatch time).

    x-e221 adds the territory keying: ``scope`` is the canonical crown scope the
    target drains (empty only on a legacy receipt), ``rung`` the crown rung,
    ``kingless`` whether no live crown holds the scope, and ``members`` the ids
    one tick converges - epic ids at rung 2, project names at rungs 0/1.
    """

    project: str
    cwd: str
    interval_seconds: int
    failure_limit: int
    mission: Optional[str]
    scope: str = ""
    rung: int = 1
    kingless: bool = True
    members: tuple[str, ...] = ()


def _workspace_paths(*, strict: bool = False) -> dict[str, str]:
    """project name -> normalized absolute path, from the workspace map.

    Reuses ``graph.maintain.load_workspaces`` so this resolver cannot drift from
    the project/cwd map ``fno backlog maintain`` / ``health`` already use.
    """
    try:
        from fno.graph.maintain import load_workspaces

        return load_workspaces()
    except Exception:
        if strict:
            raise
        return {}


def _active_missions(*, strict: bool = False) -> list[dict]:
    """Epic nodes with ``mission_active=true`` (K1's durable activation record),
    across all projects. The field ``fno backlog advance --epic`` sets/clears;
    a store read fault (or an external backend selection, which can never carry
    a footnote-set activation flag) yields none by default. Strict callers raise
    on the same read failures so a receipt can distinguish unknown from empty."""
    try:
        from fno.tracker.metadata import read_entries

        entries = read_entries("active_backlog")
        if not isinstance(entries, list):
            if strict:
                raise ValueError("active mission read returned a non-list")
            return []
        # Require str id + project: a non-str id would pass a truthy check but
        # raise when resolve_drain_targets sorts by id, which would disable ALL
        # target resolution on one malformed record (fail-safe: skip it instead).
        return [
            e
            for e in entries
            if isinstance(e, dict)
            and e.get("mission_active") is True
            and isinstance(e.get("id"), str)
            and isinstance(e.get("project"), str)
        ]
    except Exception:  # noqa: BLE001 - a graph read/iterate fault yields no missions
        if strict:
            raise
        return []


def _live_crowns(*, strict: bool = False) -> list[dict]:
    """Live crown scopes from the registry cache (x-f0d2: the row is a cache;
    the daemon reads it as court does). One ``{scope, level}`` per DISTINCT
    canonical scope, in scope order. A registry read fault yields none by
    default; strict callers raise so a receipt can distinguish unknown from
    empty."""
    try:
        from fno.agents.crown import canonical_scope, split_scope
        from fno.agents.registry import TERMINAL_STATUSES, load_registry

        rows = load_registry()
    except Exception:  # noqa: BLE001 - an unreadable registry drains nothing
        if strict:
            raise
        return []
    out: list[dict] = []
    seen: set[str] = set()
    for row in rows:
        scope = getattr(row, "crown_scope", None)
        if not scope or getattr(row, "status", "") in TERMINAL_STATUSES:
            continue
        canon = canonical_scope(split_scope(scope))
        if not canon or canon in seen:
            continue
        seen.add(canon)
        try:
            level = int(getattr(row, "crown_level", 0) or 0)
        except (TypeError, ValueError):
            level = 0
        out.append(
            {
                "scope": canon,
                "level": level,
                "holder": str(getattr(row, "name", "") or ""),
            }
        )
    out.sort(key=lambda c: c["scope"])
    return out


def _territories(*, strict: bool = False) -> list[dict]:
    """The territory list (x-e221): one entry per live crown scope plus one
    rung-1 territory per workspace project no live project-rung crown rules.
    A crownless territory still drains - ``kingless`` only names it in the
    readout. Fail-safe: partial reads shrink the list, never raise."""
    territories: list[dict] = []
    ruled_projects: set[str] = set()
    from fno.agents.crown import _canonical_project, split_scope

    for crown in _live_crowns(strict=strict):
        members = split_scope(crown["scope"])
        if crown["level"] != 2:
            ruled_projects.update(members)
        territories.append(
            {
                "scope": crown["scope"],
                "rung": crown["level"],
                "kingless": False,
                "members": members,
            }
        )
    try:
        projects = _workspace_paths(strict=strict) if strict else _workspace_paths()
    except Exception:  # noqa: BLE001 - no workspace map: crowned territories only
        projects = {}
    for name in sorted(projects):
        try:
            canon = _canonical_project(name) or name
        except Exception:  # noqa: BLE001 - a config fault degrades to the raw name
            canon = name
        if canon in ruled_projects:
            continue
        territories.append(
            {
                "scope": canon,
                "rung": 1,
                "kingless": True,
                "members": [canon],
            }
        )
    return territories


def _epic_project(epic_id: str) -> Optional[str]:
    """The project an epic node lives in, or None when unreadable/unmapped."""
    try:
        from fno.tracker.metadata import read_entries

        entries = read_entries("active_backlog")
        for row in entries:
            if isinstance(row, dict) and row.get("id") == epic_id:
                project = row.get("project")
                return project if isinstance(project, str) and project else None
    except Exception:  # noqa: BLE001 - a graph read fault roots nothing
        pass
    return None


def resolve_drain_targets(*, strict: bool = False) -> list[DrainTarget]:
    """One drain target per TERRITORY, in scope order (x-e221).

    The crown list seeds the mission list: one target per live crown scope
    (``mission == crown scope``; a scope with no live crown still drains and
    the readout names it kingless), plus one rung-1 territory per workspace
    project no live project-rung crown rules, draining that project's loose
    nodes independently of the epic territories. ``mission_active`` stays
    K1's record for ``advance --epic``'s own bookkeeping; it no longer gates
    the daemon's target list.

    ``config.active_backlog`` stays the daemon's master switch: an unenabled
    config or invalid interval yields no targets. A territory that cannot be
    rooted (a rung-2 scope whose first member epic has no workspace path, an
    unmapped project) is skipped. Fail-safe throughout.
    """
    try:
        from fno.config import load_settings

        cfg = load_settings().active_backlog
    except Exception:
        if strict:
            raise
        return []

    if not cfg.any_enabled():
        return []
    interval = cfg.interval_seconds()
    if interval is None:
        return []

    paths = _workspace_paths(strict=True) if strict else _workspace_paths()
    targets: list[DrainTarget] = []
    for territory in sorted(_territories(strict=strict), key=lambda t: t["scope"]):
        members = territory["members"]
        if territory["rung"] == 2:
            # Root at the first member epic's own project; the converge core
            # fans out across projects at dispatch time.
            root_project = _epic_project(members[0]) if members else None
            mission = members[0] if members else None
        else:
            root_project = members[0] if members else None
            mission = None
        if not root_project:
            continue
        # Respect the per-project enable contract: with enabled={proj: bool} an
        # explicitly-disabled project's territory does not drain, even though
        # any_enabled() is true for the daemon as a whole.
        if not cfg.is_enabled_for(root_project):
            continue
        cwd = paths.get(root_project)
        if not cwd:
            continue
        targets.append(
            DrainTarget(
                project=root_project,
                cwd=cwd,
                interval_seconds=interval,
                failure_limit=cfg.failure_limit,
                mission=mission,
                scope=territory["scope"],
                rung=territory["rung"],
                kingless=territory["kingless"],
                members=tuple(members),
            )
        )
    return targets


@dataclass
class FanoutTarget:
    """A project the status-fanout supervisor should tick (x-2057). Enablement is
    'has >=1 enabled status sink', INDEPENDENT of active_backlog drain."""

    project: str
    cwd: str
    interval_seconds: int


def resolve_fanout_targets() -> list["FanoutTarget"]:
    """Projects with at least one enabled status sink, each carrying its own
    ``status_fanout.interval_secs``. Reuses the same workspace project->path map
    as the drain resolver; a project without a workspace path is skipped (no cwd
    to tick from - the standalone/cron ``fno doctor event fanout tick`` covers a
    runner-less setup)."""
    from pathlib import Path as _P

    from fno.config import load_settings_for_repo

    targets: list[FanoutTarget] = []
    for name, cwd in sorted(_workspace_paths().items()):
        if not cwd:
            continue
        try:
            settings = load_settings_for_repo(_P(cwd))
        except Exception:  # noqa: BLE001 - a bad/absent settings must not tick
            continue
        if not any(s.enabled for s in settings.status_sinks):
            continue
        targets.append(
            FanoutTarget(
                project=name,
                cwd=cwd,
                interval_seconds=max(1, int(settings.status_fanout.interval_secs)),
            )
        )
    return targets


def fanout_targets_as_dicts() -> list[dict]:
    """JSON-serializable form of :func:`resolve_fanout_targets` for the daemon."""
    return [
        {"project": t.project, "cwd": t.cwd, "interval_seconds": t.interval_seconds}
        for t in resolve_fanout_targets()
    ]


def drain_targets_as_dicts() -> list[dict]:
    """JSON-serializable form of :func:`resolve_drain_targets` for the daemon.

    The mission drain shells ``advance --epic`` / ``advance --loose``, which
    resolve each child project's ``batch`` / ``max_lanes`` themselves - so,
    unlike the deleted per-project arm, the target carries no per-repo
    dispatch config. The territory fields (x-e221) ride additively: a receipt
    consumer older than them reads the legacy fields only."""
    return [
        {
            "project": t.project,
            "cwd": t.cwd,
            "interval_seconds": t.interval_seconds,
            "failure_limit": t.failure_limit,
            "mission": t.mission,
            "scope": t.scope,
            "rung": t.rung,
            "kingless": t.kingless,
            "members": list(t.members),
        }
        for t in resolve_drain_targets()
    ]


def territory_rows(*, strict: bool = False) -> list[dict]:
    """One readout row per territory (x-e221 AC7): the projection status, the
    king check-in, and the operational probe share so none of them can
    disagree with another.

    A row names the canonical scope, its mission ids (epic members; a project
    territory has none), the crown holder or an explicit kingless state, the
    live node-working count against ``agents.max_live_per_territory``, and
    the standing blueprinter's handle with its delivery totals. Read-only and
    fail-safe: an unreadable source shrinks that row's answer (membership
    ``unknown``, live ``None``, no blueprinter), never the whole projection.
    """
    try:
        from fno.config import load_settings

        cap = int(getattr(load_settings().agents, "max_live_per_territory", 4))
    except Exception:  # noqa: BLE001 - an unreadable config keeps the default
        cap = 4

    crowns = {c["scope"]: c for c in _live_crowns(strict=strict)}
    try:
        from fno.agents.spawn_gate import census

        census_live = census()
        live_names = set(census_live.live_registry_names)
        live_nodes = [n for n in census_live.live_row_nodes if n]
    except Exception:  # noqa: BLE001 - an unreadable census reads as no rows
        live_names, live_nodes = set(), []
    try:
        from fno.graph.store import read_graph
        from fno import paths as _paths

        entries = read_graph(_paths.graph_json())
        if not isinstance(entries, list):
            entries = []
    except Exception:  # noqa: BLE001 - an unreadable graph reads as unknown
        entries = []

    from fno.king.scope import territory_membership
    from fno.worker.blueprint import _read_record

    # Exclusive membership (x-e221): a crowned node counts for its crown
    # scope only. A kingless project territory's ids drop every node a live
    # crown already claims, so a worker can never cost two territories at
    # once - the same rule the spawn gate enforces.
    memberships: list = []
    crown_ids: set = set()
    for territory in _territories(strict=strict):
        tm = territory_membership(territory["scope"], entries)
        memberships.append((territory, tm))
        if not territory["kingless"] and tm.state == "ok":
            crown_ids |= tm.ids

    rows: list[dict] = []
    for territory, tm in memberships:
        scope = territory["scope"]
        tm_key = (tm.key or scope) if tm.state == "ok" else scope
        ids = tm.ids if tm.state == "ok" else frozenset()
        if territory["kingless"]:
            ids = ids - crown_ids
        live: "int | None" = None
        if tm.state == "ok":
            live = sum(1 for n in live_nodes if n in ids)
        record = _read_record(tm_key)
        worker = record.get("worker") or None
        blueprinter = None
        if worker:
            blueprinter = {
                "name": worker.get("name"),
                "live": worker.get("name") in live_names,
                "spawned_at": worker.get("spawned_at"),
                "fed": len(record.get("fed") or {}),
                "repairs": len(record.get("repairs") or []),
            }
        rows.append(
            {
                "scope": tm_key,
                "membership": tm.state,
                "rung": territory["rung"],
                "kingless": territory["kingless"],
                "holder": crowns.get(scope, {}).get("holder"),
                "mission": territory["members"][0] if territory["rung"] == 2 and territory["members"] else None,
                "live": live,
                "cap": cap,
                "blueprinter": blueprinter,
            }
        )
    return rows
