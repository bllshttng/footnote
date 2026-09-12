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
    #: Global ceiling on concurrent converge runs across ALL missions, not a
    #: per-mission budget. Every target carries the same value because the
    #: daemon holds one gate for the whole drain; it rides on the target only
    #: because the target list is the daemon's one config channel.
    max_concurrent: int = 1


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


@dataclass(frozen=True)
class DrainReading:
    """What the drain resolver saw, not only what it resolved (x-338c).

    ``missions`` counts active missions whatever the config says, so a
    switched-off drain still shows the work it is not doing. ``skip_reason``
    names which zero-path produced an empty target list; ``None`` when
    ``targets`` is non-empty.
    """

    targets: list[DrainTarget]
    missions: int
    skip_reason: Optional[str]


def resolve_drain_reading(*, strict: bool = False) -> DrainReading:
    """One reading of the drain: targets, mission count, and which zero hit.

    Same resolver contract as :func:`resolve_drain_targets` (which returns only
    ``.targets``), plus the reason channel: a disabled drain and a drain with
    no missions are different facts with different remedies, and both resolved
    to the same empty list before (x-338c). Zero-paths, in the order checked:

    =======================  =====================
    Condition                ``skip_reason``
    =======================  =====================
    ``load_settings`` fault  ``config_unreadable``
    every-project disabled   ``drain_disabled``
    invalid interval         ``bad_interval``
    no active missions       ``no_missions``
    all dropped per-project  ``project_disabled``
    all dropped, no path     ``no_workspace_path``
    =======================  =====================

    Missions are counted BEFORE the config gates so ``missions`` is the truth
    even when the drain is off; a strict mission-read fault raises, keeping
    ``_active_missions``' strict contract. When missions drop for mixed
    per-mission reasons the most common drop wins, ties to the table order.
    """
    missions = _active_missions(strict=True) if strict else _active_missions()
    try:
        from fno.config import load_settings

        cfg = load_settings().active_backlog
    except Exception:
        if strict:
            raise
        return DrainReading([], len(missions), "config_unreadable")

    # The master switch, read apart from the interval: any_enabled() folds the
    # two, which would make an off switch and a bad interval indistinguishable.
    en = cfg.enabled
    switch_on = any(en.values()) if isinstance(en, dict) else bool(en)
    if not switch_on:
        return DrainReading([], len(missions), "drain_disabled")
    interval = cfg.interval_seconds()
    if interval is None:
        return DrainReading([], len(missions), "bad_interval")
    if not missions:
        return DrainReading([], 0, "no_missions")

    paths = _workspace_paths(strict=True) if strict else _workspace_paths()
    targets: list[DrainTarget] = []
    disabled = missing_path = 0
    for epic in sorted(missions, key=lambda e: e["id"]):
        project = epic["project"]
        # Respect the per-project enable contract: with enabled={proj: bool} an
        # explicitly-disabled project's mission does not drain, even though
        # the switch is on for the daemon as a whole.
        if not cfg.is_enabled_for(project):
            disabled += 1
            continue
        cwd = paths.get(project)
        if not cwd:
            missing_path += 1
            continue
        targets.append(
            DrainTarget(
                project=project,
                cwd=cwd,
                interval_seconds=interval,
                failure_limit=cfg.failure_limit,
                mission=epic["id"],
                max_concurrent=cfg.max_concurrent,
            )
        )
    if not targets:
        reason = "project_disabled" if disabled >= missing_path else "no_workspace_path"
        return DrainReading([], len(missions), reason)
    return DrainReading(targets, len(missions), None)


def resolve_drain_targets(*, strict: bool = False) -> list[DrainTarget]:
    """One drain target per ACTIVE mission, in epic-id order (x-a4dc K2).

    A mission is an epic with ``mission_active=true`` (K1's activation record).
    The daemon drains each by shelling K1's converge core (``advance --epic``),
    which fans out the epic's ready leaf children across ALL projects; the epic id
    rides on the target's ``mission``. The legacy per-project interval drain and
    its opt-in escape env are deleted (epic Locked Decision 4) - merge-triggered
    ``fno backlog advance`` is the same-project coverage, and no per-project drain
    ever comes back.

    ``config.active_backlog`` stays the daemon's master switch: an unenabled
    config or invalid interval yields no targets. ``config.active_backlog.mission``
    is IGNORED (x-7f1f): missions are per-epic graph state (``mission_active``),
    never a config value. A mission whose epic project has
    no workspace path is skipped (cannot root the loop). Fail-safe throughout.

    Callers that must tell the zero-paths apart read
    :func:`resolve_drain_reading` instead.
    """
    return resolve_drain_reading(strict=strict).targets


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


def drain_reading_as_dict() -> dict:
    """JSON-serializable receipt of :func:`resolve_drain_reading` for the daemon.

    The bare target list this replaces could not say WHY it was empty, so a
    disabled drain printed as ``no_missions`` (x-338c). The mission drain
    shells ``advance --epic``, which resolves each child project's ``batch`` /
    ``max_lanes`` itself - so, unlike the deleted per-project arm, the target
    carries no per-repo dispatch config."""
    reading = resolve_drain_reading()
    return {
        "targets": [
            {
                "project": t.project,
                "cwd": t.cwd,
                "interval_seconds": t.interval_seconds,
                "failure_limit": t.failure_limit,
                "mission": t.mission,
                "max_concurrent": t.max_concurrent,
            }
            for t in reading.targets
        ],
        "missions": reading.missions,
        "skip_reason": reading.skip_reason,
    }
