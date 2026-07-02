"""Active-backlog drain-target resolution (node x-c070).

Resolves which projects the always-on backlog dispatcher daemon should drain,
from ``config.active_backlog`` plus the workspace project->path map. The daemon
is a per-user global process with no inherent project, so it shells
``fno config active-backlog --json`` once on entering Serving to learn its drain
targets (cwd + cadence + failure limit + mission) - keeping all config logic in
Python, the single source of truth, exactly like the rest of the daemon's
config-ish reads.

Pure + best-effort: a malformed settings file yields no targets rather than
raising, so the daemon never crashes on an operator config typo.
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
    """One project the daemon should continuously drain."""

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


def resolve_drain_targets() -> list[DrainTarget]:
    """The projects the active-backlog daemon should drain, in name order.

    - ``enabled: true`` (bool) -> every project in the workspace map. With no
      workspace map there is no project to root a drain at, so the result is
      empty (fail-safe, never a guessed cwd).
    - ``enabled: {<project>: bool}`` -> only the truthy keys, each resolved to
      its workspace path. A name with no workspace path is skipped (cannot drain
      without a cwd).
    - An invalid interval disables everything (``any_enabled`` already returns
      False via the fail-closed accessor).
    """
    try:
        from fno.config import load_settings

        cfg = load_settings().config.active_backlog
    except Exception:
        return []

    if not cfg.any_enabled():
        return []
    interval = cfg.interval_seconds()
    if interval is None:
        return []

    paths = _workspace_paths()
    enabled = cfg.enabled
    if isinstance(enabled, dict):
        names = [p for p, on in enabled.items() if on]
    else:
        # bool True -> every workspace project.
        names = list(paths.keys())

    targets: list[DrainTarget] = []
    for name in sorted(names):
        cwd = paths.get(name)
        if not cwd:
            continue
        targets.append(
            DrainTarget(
                project=name,
                cwd=cwd,
                interval_seconds=interval,
                failure_limit=cfg.failure_limit,
                mission=cfg.mission,
            )
        )
    return targets


def _batch_enabled_for(cwd: str) -> bool:
    """config.batch.enabled for a target repo (batch-lane Wave 2, x-6cdf).

    Read per-repo so a project opts into batched dispatch independently. Fail-
    safe to False (the daemon then dispatches normal /target no-merge workers).
    """
    try:
        from pathlib import Path as _P

        from fno.config import load_settings_for_repo

        return bool(load_settings_for_repo(_P(cwd)).config.batch.enabled)
    except Exception:  # noqa: BLE001 - a bad/absent settings must not enable
        return False


def _max_lanes_for(cwd: str) -> int:
    """config.parallel.max_lanes for a target repo (parallel mode x-42d5, G4).

    Read per-repo like ``_batch_enabled_for`` so a project opts into parallel
    lane-fill independently. Fail-safe to 1 (today's sequential single-lane
    path): a bad/absent settings read must never fan a repo out into lanes.
    """
    try:
        from pathlib import Path as _P

        from fno.config import load_settings_for_repo

        # Clamp at 0: the schema already rejects negatives (ge=0), but a
        # negative escaping here would fail the Rust daemon's u64 deserialize
        # for the WHOLE target list, silently disabling the drain (gemini).
        return max(0, int(load_settings_for_repo(_P(cwd)).config.parallel.max_lanes))
    except Exception:  # noqa: BLE001 - a bad/absent settings must not go parallel
        return 1


def drain_targets_as_dicts() -> list[dict]:
    """JSON-serializable form of :func:`resolve_drain_targets` for the daemon."""
    return [
        {
            "project": t.project,
            "cwd": t.cwd,
            "interval_seconds": t.interval_seconds,
            "failure_limit": t.failure_limit,
            "mission": t.mission,
            "batch": _batch_enabled_for(t.cwd),
            "max_lanes": _max_lanes_for(t.cwd),
        }
        for t in resolve_drain_targets()
    ]
