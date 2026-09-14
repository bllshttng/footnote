"""Active-backlog state helpers that stay Python-side.

The territory fact set, the drain-target receipt, and the blueprinter record
store are native to ``crates/fno-agents/src/territory.rs`` (the x-e221 port);
``fno config active-backlog*`` prints the binary's receipt. What survives here
is the wake-nudge sentinel the graph writers touch and the status-fanout
target resolver (x-2057), which is a separate supervisor with its own
enablement contract.

Pure + best-effort: a malformed settings file yields no targets rather than
raising, so the daemon never crashes on an operator config typo.
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

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
