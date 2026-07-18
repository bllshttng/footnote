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
    """

    project: str
    cwd: str
    interval_seconds: int
    failure_limit: int
    mission: Optional[str]


def _workspace_paths() -> dict[str, str]:
    """project name -> normalized absolute path, from the workspace map.

    Reuses ``graph.maintain.load_workspaces`` so this resolver cannot drift from
    the project/cwd map ``fno backlog maintain`` / ``health`` already use.
    """
    try:
        from fno.graph.maintain import load_workspaces

        return load_workspaces()
    except Exception:
        return {}


def _active_missions() -> list[dict]:
    """Epic nodes with ``mission_active=true`` (K1's durable activation record),
    across all projects. The field ``fno backlog advance --epic`` sets/clears; a
    graph read fault yields none (fail-safe, never raises)."""
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        entries = read_graph(graph_json())
    except Exception:  # noqa: BLE001 - a graph read fault yields no missions
        return []
    return [
        e
        for e in entries
        if isinstance(e, dict)
        and e.get("mission_active") is True
        and e.get("id")
        and e.get("project")
    ]


def resolve_drain_targets() -> list[DrainTarget]:
    """One drain target per ACTIVE mission, in epic-id order (x-a4dc K2).

    A mission is an epic with ``mission_active=true`` (K1's activation record).
    The daemon drains each by shelling K1's converge core (``advance --epic``),
    which fans out the epic's ready leaf children across ALL projects; the epic id
    rides on the target's ``mission``. The legacy per-project interval drain and
    its opt-in escape env are deleted (epic Locked Decision 4) - merge-triggered
    ``fno backlog advance`` is the same-project coverage, and no per-project drain
    ever comes back.

    ``config.active_backlog`` stays the daemon's master switch: an unenabled
    config or invalid interval yields no targets. A mission whose epic project has
    no workspace path is skipped (cannot root the loop). Fail-safe throughout.
    """
    try:
        from fno.config import load_settings

        cfg = load_settings().active_backlog
    except Exception:
        return []

    if not cfg.any_enabled():
        return []
    interval = cfg.interval_seconds()
    if interval is None:
        return []

    paths = _workspace_paths()
    targets: list[DrainTarget] = []
    for epic in sorted(_active_missions(), key=lambda e: e["id"]):
        cwd = paths.get(epic["project"])
        if not cwd:
            continue
        targets.append(
            DrainTarget(
                project=epic["project"],
                cwd=cwd,
                interval_seconds=interval,
                failure_limit=cfg.failure_limit,
                mission=epic["id"],
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
    to tick from - the standalone/cron ``fno status-fanout tick`` covers a
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

    The mission drain shells ``advance --epic``, which resolves each child
    project's ``batch`` / ``max_lanes`` itself - so, unlike the deleted per-project
    arm, the target carries no per-repo dispatch config."""
    return [
        {
            "project": t.project,
            "cwd": t.cwd,
            "interval_seconds": t.interval_seconds,
            "failure_limit": t.failure_limit,
            "mission": t.mission,
        }
        for t in resolve_drain_targets()
    ]
