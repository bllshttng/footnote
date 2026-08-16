"""fno autonomy status - one table listing every path that can start a
session without an operator asking: its trigger, its gate key, its resolved
value, and the precedence rank that supplied it (x-aaaf wave 1).

The deliverable an operator actually asked for: "what can spawn without me"
answerable in one command, buildable before any gate is unified (wave 3).

Each spawner is resolved THROUGH ITS REAL RESOLVER, never by reading config
directly - a status verb that read config.auto_continue.enabled directly
would have printed "false" throughout the six-and-a-half-week incident that
motivated this verb, because a since-removed marker file silently outranked
it. A spawner with no gate at all (groom / restart / evals, until wave 2
gates them) renders as ``ungated`` rather than being omitted from the table -
omission is exactly what hid them.

Exit code is always 0 - this is introspection, never a gate (AC9-ERR).
"""
from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import typer

autonomy_app = typer.Typer(
    name="autonomy",
    help="Inspect every path that can start a session without an operator asking.",
    no_args_is_help=True,
)


@dataclass(frozen=True)
class SpawnerStatus:
    """One row of the autonomy table."""

    name: str
    trigger: str
    gate_key: str
    armed: Optional[bool]  # None = ungated (no gate exists yet)
    rank: str  # "env" | "config" | "default" | "autonomy" | "ungated"


def _settings_for(project_root: Optional[Path]):
    from fno.config import load_settings, load_settings_for_repo

    return load_settings_for_repo(Path(project_root)) if project_root else load_settings()


def _master_switch_status(project_root: Optional[Path]) -> SpawnerStatus:
    """Rank reflects whether the settings read succeeded, not the resolved
    value - the same convention every other row uses (armed=False from a
    successful read is still rank "config"; only a read failure is
    "default"). Mapping the boolean straight to a rank would label an
    explicit ``autonomy.enabled=false`` as "default", which reads as
    unconfigured precisely when an operator used the panic switch."""
    from fno.config import load_settings, load_settings_for_repo

    try:
        settings = load_settings_for_repo(Path(project_root)) if project_root else load_settings()
        armed = bool(settings.autonomy.enabled)
        rank = "config"
    except Exception:  # noqa: BLE001 - fail-safe to disabled on a read failure
        armed = False
        rank = "default"
    return SpawnerStatus(
        "autonomy (master switch)", "every row below", "config.autonomy.enabled", armed,
        rank,
    )


def _gate_with_master(
    project_root: Optional[Path], read_value
) -> tuple[Optional[bool], str]:
    """Shared shape for the direct-config-read rows below: the master panic
    switch is checked FIRST (rank "autonomy" when it vetoes), matching the
    precedence every resolver-backed spawner already applies to itself."""
    from fno.config import autonomy_master_enabled

    if not autonomy_master_enabled(project_root):
        return False, "autonomy"
    return bool(read_value()), "config"


def _advance_status(project_root: Optional[Path]) -> SpawnerStatus:
    from fno.backlog.advance import _auto_continue_resolve

    armed, rank = _auto_continue_resolve(project_root)
    return SpawnerStatus(
        "advance (node-walk)", "PR merge (reconcile / /pr merged)",
        "config.auto_continue.enabled", armed, rank,
    )


def _dispatch_lanes_status(project_root: Optional[Path]) -> SpawnerStatus:
    # dispatch_lanes has no gate check of its own; every caller invokes it
    # only after auto_continue_enabled() already passed (design doc,
    # x-aaaf), so its armed state and rank are the SAME resolution.
    from fno.backlog.advance import _auto_continue_resolve

    armed, rank = _auto_continue_resolve(project_root)
    return SpawnerStatus(
        "dispatch_lanes (parallel fill)", "active-backlog daemon tick",
        "config.auto_continue.enabled", armed, rank,
    )


def _epic_converge_status(project_root: Optional[Path]) -> SpawnerStatus:
    from fno.backlog.advance import _auto_continue_resolve

    armed, rank = _auto_continue_resolve(project_root)
    return SpawnerStatus(
        "epic converge (mission drain)", "fno backlog advance --epic",
        "config.auto_continue.enabled", armed, rank,
    )


def _reconcile_dispatch_status(project_root: Optional[Path]) -> SpawnerStatus:
    from fno.backlog.advance import _auto_continue_resolve

    armed, rank = _auto_continue_resolve(project_root)
    return SpawnerStatus(
        "reconcile_dispatch (G4 de-stub)", "PR merge (contract dependent's blocker closes)",
        "config.auto_continue.enabled", armed, rank,
    )


def _spawn_think_status(project_root: Optional[Path]) -> SpawnerStatus:
    from fno.provenance.spawn_think import _think_spawn_resolve

    armed, rank = _think_spawn_resolve(project_root=project_root)
    return SpawnerStatus(
        "spawn_think (context /think)", "node birth / work-start / retro",
        "config.think_spawn.enabled", armed, rank,
    )


def _post_merge_status(project_root: Optional[Path]) -> SpawnerStatus:
    try:
        armed, rank = _gate_with_master(
            project_root, lambda: _settings_for(project_root).post_merge.enabled
        )
        return SpawnerStatus(
            "post-merge ritual", "PR merge", "config.post_merge.enabled", armed, rank,
        )
    except Exception:  # noqa: BLE001 - AC9-ERR: never let one row crash the table
        return SpawnerStatus(
            "post-merge ritual", "PR merge", "config.post_merge.enabled", False, "default",
        )


def _pr_watch_status(project_root: Optional[Path]) -> SpawnerStatus:
    try:
        armed, rank = _gate_with_master(
            project_root, lambda: _settings_for(project_root).pr_watch.enabled
        )
        return SpawnerStatus(
            "pr_watch (headless PR poll)", "launchd interval tick",
            "config.pr_watch.enabled", armed, rank,
        )
    except Exception:  # noqa: BLE001
        return SpawnerStatus(
            "pr_watch (headless PR poll)", "launchd interval tick",
            "config.pr_watch.enabled", False, "default",
        )


def _recovery_status(project_root: Optional[Path]) -> SpawnerStatus:
    """x-aaaf wave 3: found while building the registry ratchet (task 3.3) --
    recovery.py's crash-recovery respawn had a real config.recovery.enabled
    gate but was reachable with the master switch off (pr_watch/cli.py checked
    recovery.enabled alone, never autonomy.enabled). Fixed alongside this row."""
    try:
        armed, rank = _gate_with_master(
            project_root, lambda: _settings_for(project_root).recovery.enabled
        )
        return SpawnerStatus(
            "recovery sweep (crash respawn)", "pr_watch launchd interval tick",
            "config.recovery.enabled", armed, rank,
        )
    except Exception:  # noqa: BLE001
        return SpawnerStatus(
            "recovery sweep (crash respawn)", "pr_watch launchd interval tick",
            "config.recovery.enabled", False, "default",
        )


def _keep_going_status(project_root: Optional[Path]) -> SpawnerStatus:
    """keep_going_enabled() swallows its own settings-read exception and
    returns a plain False with no rank exposed - calling it here would report
    whatever rank was guessed BEFORE the call (usually "config"), even when
    the real cause was an unreadable settings file. Resolve inline instead,
    like _auto_continue_resolve, so a read failure genuinely reports rank
    "default" (the exact failure class this module's docstring cites)."""
    from fno.config import autonomy_master_enabled
    from fno.retro.keep_going import _ENV_OVERRIDE, _TRUTHY

    try:
        if not autonomy_master_enabled(project_root):
            armed, rank = False, "autonomy"
        else:
            env = os.environ.get(_ENV_OVERRIDE)
            if env is not None:
                armed, rank = env.strip().lower() in _TRUTHY, "env"
            else:
                settings = _settings_for(project_root)
                armed, rank = bool(settings.keep_going.enabled), "config"
    except Exception:  # noqa: BLE001
        armed, rank = False, "default"
    return SpawnerStatus(
        "keep_going (autonomous follow-up)", "post-merge ritual carve-out drain",
        "config.keep_going.enabled", armed, rank,
    )


def _blueprint_auto_launch_status(project_root: Optional[Path]) -> SpawnerStatus:
    try:
        armed, rank = _gate_with_master(
            project_root, lambda: _settings_for(project_root).target.auto_launch_on_blueprint
        )
        return SpawnerStatus(
            "blueprint auto-launch", "/blueprint completion",
            "config.target.auto_launch_on_blueprint", armed, rank,
        )
    except Exception:  # noqa: BLE001
        return SpawnerStatus(
            "blueprint auto-launch", "/blueprint completion",
            "config.target.auto_launch_on_blueprint", False, "default",
        )


def _groom_status(project_root: Optional[Path]) -> SpawnerStatus:
    try:
        armed, rank = _gate_with_master(
            project_root, lambda: _settings_for(project_root).groom.enabled
        )
        return SpawnerStatus(
            "groom (_spawn_groom_worker)", "fno backlog groom",
            "config.groom.enabled", armed, rank,
        )
    except Exception:  # noqa: BLE001
        return SpawnerStatus(
            "groom (_spawn_groom_worker)", "fno backlog groom",
            "config.groom.enabled", False, "default",
        )


def _restart_status(project_root: Optional[Path]) -> SpawnerStatus:
    try:
        armed, rank = _gate_with_master(
            project_root, lambda: _settings_for(project_root).restart.enabled
        )
        return SpawnerStatus(
            "restart (_revive_orphans)", "orphan sweep (fno restart --mux)",
            "config.restart.enabled", armed, rank,
        )
    except Exception:  # noqa: BLE001
        return SpawnerStatus(
            "restart (_revive_orphans)", "orphan sweep (fno restart --mux)",
            "config.restart.enabled", False, "default",
        )


def _evals_status(project_root: Optional[Path]) -> SpawnerStatus:
    try:
        armed, rank = _gate_with_master(
            project_root, lambda: _settings_for(project_root).evals.enabled
        )
        return SpawnerStatus(
            "evals runner", "fno evals run", "config.evals.enabled", armed, rank,
        )
    except Exception:  # noqa: BLE001
        return SpawnerStatus(
            "evals runner", "fno evals run", "config.evals.enabled", False, "default",
        )


def collect_status(project_root: Optional[Path] = None) -> list[SpawnerStatus]:
    """Resolve every known spawner's armed state through its real resolver."""
    rows = [
        _master_switch_status(project_root),
        _advance_status(project_root),
        _dispatch_lanes_status(project_root),
        _epic_converge_status(project_root),
        _reconcile_dispatch_status(project_root),
        _spawn_think_status(project_root),
        _post_merge_status(project_root),
        _pr_watch_status(project_root),
        _recovery_status(project_root),
        _keep_going_status(project_root),
        _blueprint_auto_launch_status(project_root),
        # x-aaaf wave 2: previously ungated, now gated - see GroomBlock /
        # RestartBlock / EvalsBlock in fno.config.
        _groom_status(project_root),
        _restart_status(project_root),
        _evals_status(project_root),
    ]
    return rows


def format_table(rows: list[SpawnerStatus]) -> str:
    headers = ("SPAWNER", "TRIGGER", "GATE KEY", "ARMED", "RANK")

    def _armed_cell(v: Optional[bool]) -> str:
        if v is None:
            return "ungated"
        return "true" if v else "false"

    table_rows = [
        (r.name, r.trigger, r.gate_key, _armed_cell(r.armed), r.rank) for r in rows
    ]
    widths = [
        max(len(headers[i]), *(len(row[i]) for row in table_rows)) if table_rows else len(headers[i])
        for i in range(len(headers))
    ]
    lines = [
        "  ".join(h.ljust(widths[i]) for i, h in enumerate(headers)),
        "  ".join("-" * widths[i] for i in range(len(headers))),
    ]
    lines.extend(
        "  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)) for row in table_rows
    )
    return "\n".join(lines)


@autonomy_app.command("status")
def status_command(
    project_root: Optional[Path] = typer.Option(
        None, "--project-root", help="Repo root to resolve config against (default: cwd).",
    ),
) -> None:
    """List every spawner, its trigger, its gate, and its resolved value.

    Always exits 0 (AC9-ERR): this is a read, never a gate. Even a repo with
    no fno config at all prints every spawner's fail-safe default rather
    than raising.
    """
    try:
        rows = collect_status(project_root)
        typer.echo(format_table(rows))
    except Exception as exc:  # noqa: BLE001 - AC9-ERR: introspection must not raise
        typer.echo(f"fno autonomy status: degraded read ({exc})", err=True)
    raise typer.Exit(code=0)
