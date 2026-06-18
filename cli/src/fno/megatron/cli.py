"""fno megatron CLI: run, next, complete, status, cancel, retro, list, reconcile.

The CLI is a thin Typer surface. ``run`` is a strangler front door over the
unified Rust loop (``fno-agents loop run --driver megatron``); ``next`` /
``complete`` are the mission-queue plumbing verbs that loop shells (see
``queue.py``). Mission resolution walks ``~/.fno/fleet/*/state.md``
looking for a matching ``mission_id`` line in the YAML frontmatter; the
first match wins. The same path layout houses the cancel sentinel
(``{slug}/.cancelled``); cancellation propagates to a running commander
through the verb seam (``next`` returns a terminal pause).
"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import typer

from fno.megatron._constants import TERMINAL_STATUSES
from fno.megatron.manifest import ManifestError, load_manifest
from fno.megatron.state import (
    MissionStateCorrupt,
    read_state,
    resolve_mission_directory,
    update_status,
    write_state,
)


app = typer.Typer(no_args_is_help=True, add_completion=False)


def _fleet_root() -> Path:
    from fno import paths as _paths
    return _paths.fleet_dir()


def _resolve_mission(mission_id: str) -> Optional[Path]:
    """Return the fleet directory for ``mission_id`` or None.

    Thin wrapper over ``state.resolve_mission_directory`` so callers
    inside this module read locally; the canonical implementation
    lives in state.py per Gemini MEDIUM finding on PR #216.
    """
    return resolve_mission_directory(mission_id, _fleet_root())


def _resolve_driver_lib_dir() -> Optional[Path]:
    """Resolve the driver-lib directory for the Rust loop, or None.

    `fno megatron run` is invoked from arbitrary cwds (any project, any
    mission), but the Rust loop's fallback is `<cwd>/scripts/lib` - which
    only exists inside an fno checkout (codex P2 on PR #458). Resolve
    Python-side while we have the context: env wins (return None and let the
    Rust side read it), then the plugin root, then the source checkout this
    package sits in. None with no env set lets the Rust side surface its
    actionable error.
    """
    import os

    if os.environ.get("FNO_DRIVER_LIB_DIR"):
        return None  # the Rust side honors the env var directly
    candidates = []
    plugin_root = os.environ.get("CLAUDE_PLUGIN_ROOT", "")
    if plugin_root:
        candidates.append(Path(plugin_root) / "scripts" / "lib")
    # Dev checkout: cli/src/fno/megatron/cli.py -> repo root is parents[4].
    try:
        candidates.append(Path(__file__).resolve().parents[4] / "scripts" / "lib")
    except IndexError:
        pass
    candidates.append(Path.cwd() / "scripts" / "lib")
    for c in candidates:
        if c.is_dir():
            return c
    return None


def _resolve_loop_binary() -> Optional[Path]:
    """Locate the ``fno-agents`` binary that hosts the unified loop.

    ``$FNO_AGENTS_BIN`` wins (the stop-hook shim's override, reused here for
    tests and pinned installs), then the standard resolution chain (bundled
    wheel dir -> launcher sibling -> PATH -> cargo dev target).
    """
    import os

    env_bin = os.environ.get("FNO_AGENTS_BIN", "")
    if env_bin:
        p = Path(env_bin)
        if p.is_file() and os.access(p, os.X_OK):
            return p
        return None
    from fno.agents.rust_runtime import resolve_binary

    return resolve_binary()


@app.command("run")
def cmd_run(
    mission_id: str = typer.Argument(..., help="Mission id (ab-XXXX) to execute"),
    max_iterations: Optional[int] = typer.Option(
        None,
        "--max-iterations",
        help="Cap mission-level loop iterations (default: Rust loop's ceiling)",
    ),
    poll_interval_s: Optional[float] = typer.Option(
        None,
        "--poll-interval",
        help="DEPRECATED: the unified loop is journal-driven; accepted and ignored",
    ),
    combo: Optional[str] = typer.Option(
        None,
        "--combo",
        help=(
            "Use a provider combo for spawned megawalk subprocesses. CLI flag "
            "wins over the manifest's combo: key (CG7). Sets TARGET_COMBO=<name> "
            "in env (Plan B, ab-0e5a921e)."
        ),
    ),
) -> None:
    """Run mission ``mission_id`` to completion (or paused/cancelled).

    Strangler front door (group 3, ab-9fd662c6): execs the unified Rust loop
    (``fno-agents loop run --driver megatron``). The commander poll loop is
    gone; each project is walked directly (a megawalk one altitude down) and
    completion evidence is the child walk's journaled termination event.

    Exit codes (unchanged contract): 0 complete, 2 not-found/usage, 3
    commander already running, 4 paused, 130 interrupted.
    """
    fleet_dir = _resolve_mission(mission_id)
    if fleet_dir is None:
        typer.echo(f"Mission {mission_id} not found in {_fleet_root()}/", err=True)
        raise typer.Exit(code=2)

    manifest_path = fleet_dir / "00-INDEX.md"

    try:
        manifest = load_manifest(manifest_path)  # surface parse errors before launching
    except ManifestError as exc:
        typer.echo(f"Manifest error: {exc}", err=True)
        raise typer.Exit(code=2) from exc

    # CG5/CG7 (Plan B, ab-0e5a921e): CLI --combo wins over manifest combo:.
    # Resolution priority: CLI flag > manifest > env > settings (downstream
    # via sigma_dispatch.resolve_dispatch_target).
    effective_combo = combo or getattr(manifest, "combo", None)
    if effective_combo:
        from fno.adapters.providers.loader import load_combos
        try:
            combos = load_combos()
        except Exception as exc:
            typer.echo(f"error: failed to load combos: {exc}", err=True)
            raise typer.Exit(code=2)
        if effective_combo not in combos:
            typer.echo(
                f"error: combo {effective_combo!r} not found in "
                f"config.providers.combos. Known: {sorted(combos)}",
                err=True,
            )
            raise typer.Exit(code=2)
        import os as _os
        _os.environ["TARGET_COMBO"] = effective_combo

    if poll_interval_s is not None:
        typer.echo(
            "megatron run: --poll-interval is ignored (the unified loop is "
            "journal-driven; there is no polling cycle)",
            err=True,
        )

    binary = _resolve_loop_binary()
    if binary is None:
        typer.echo(
            "megatron run: fno-agents binary not found. Install it with "
            "`fno update --rust` (or `cargo build --release` in "
            "crates/fno-agents for a dev checkout), or set FNO_AGENTS_BIN.",
            err=True,
        )
        raise typer.Exit(code=2)

    cmd = [str(binary), "loop", "run", "--driver", "megatron", "--mission", mission_id]
    if max_iterations is not None:
        cmd += ["--max-iterations", str(max_iterations)]
    lib_dir = _resolve_driver_lib_dir()
    if lib_dir is not None:
        cmd += ["--driver-lib-dir", str(lib_dir)]

    import subprocess

    rc = subprocess.run(cmd, check=False).returncode

    if rc == 0:
        typer.echo(f"Mission {mission_id} complete.")
        return
    if rc == 3:
        typer.echo(
            f"megatron: another commander is already running mission {mission_id}",
            err=True,
        )
        raise typer.Exit(code=3)
    if rc == 4:
        typer.echo(
            f"Mission {mission_id} paused; see `fno megatron status {mission_id}` "
            f"and the events journal for the typed pause reason.",
            err=True,
        )
        raise typer.Exit(code=4)
    # Pass-through codes (1 budget, 2 child usage/config, 77 missing driver
    # binary, 130 interrupt): the child already printed its context; one
    # framing line attributes the exit to the loop rather than this wrapper.
    typer.echo(f"megatron run: loop exited {rc}; see output above.", err=True)
    raise typer.Exit(code=rc)


@app.command("next")
def cmd_next(
    mission_id: str = typer.Argument(..., help="Mission id (ab-XXXX) to advance"),
    as_json: bool = typer.Option(
        True,
        "--json",
        "-J",
        help="Emit machine-readable JSON (the Rust MegatronQueue's contract)",
    ),
) -> None:
    """Return the next incomplete project unit for a mission (loop plumbing).

    Dispatch-on-demand: every un-dispatched project of the current wave is
    dispatched (plan file + ``fno backlog intake``) before the first
    incomplete project is returned. Output contract (consumed by
    ``fno-agents loop run --driver megatron``):

    - unit JSON object  -> dispatch this project
    - ``null``          -> mission complete (drained)
    - ``{"pause": {"policy": ..., "detail": ...}}`` -> walk must pause
    """
    fleet_dir = _resolve_mission(mission_id)
    if fleet_dir is None:
        typer.echo(f"Mission {mission_id} not found in {_fleet_root()}/", err=True)
        raise typer.Exit(code=2)

    from fno.megatron import queue as mission_queue

    try:
        # fleet_root anchors the completion-record rebuild to the SAME root
        # the mission was resolved from (never the global default, which can
        # diverge under config.paths overrides or in tests).
        out = mission_queue.mission_next(
            fleet_dir / "00-INDEX.md",
            fleet_dir / "state.md",
            fleet_root=fleet_dir.parent,
        )
    except ManifestError as exc:
        typer.echo(f"Manifest error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    except MissionStateCorrupt as exc:
        typer.echo(f"Mission state corrupt: {exc}", err=True)
        raise typer.Exit(code=4) from exc

    del as_json  # JSON is the only output shape; flag kept for surface clarity
    if out["kind"] == "drained":
        typer.echo("null")
    elif out["kind"] == "pause":
        typer.echo(
            json.dumps({"pause": {"policy": out["policy"], "detail": out["detail"]}})
        )
    else:
        typer.echo(json.dumps(out["unit"]))


@app.command("complete")
def cmd_complete(
    mission_id: str = typer.Argument(..., help="Mission id (ab-XXXX)"),
    project: str = typer.Option(..., "--project", help="Project name (manifest or canonical)"),
    wave: int = typer.Option(..., "--wave", help="Wave number (1-based)"),
    outcome: str = typer.Option(
        ..., "--outcome", help="'done' (walk drained) or 'failed' (pause mission)"
    ),
    reason: str = typer.Option(
        "",
        "--reason",
        "-R",
        help="Termination reason from the child walk's journal event",
    ),
) -> None:
    """Record a project walk's outcome against a mission (loop plumbing).

    ``--outcome done`` idempotently writes the completion JSON (a
    worker-written record is never clobbered) and stamps the mission
    complete when it was the last record. ``--outcome failed`` pauses the
    mission with a typed reason.
    """
    fleet_dir = _resolve_mission(mission_id)
    if fleet_dir is None:
        typer.echo(f"Mission {mission_id} not found in {_fleet_root()}/", err=True)
        raise typer.Exit(code=2)

    if outcome not in ("done", "failed"):
        typer.echo(f"--outcome must be 'done' or 'failed', got {outcome!r}", err=True)
        raise typer.Exit(code=2)

    from fno.megatron import queue as mission_queue

    try:
        out = mission_queue.mission_complete(
            fleet_dir / "00-INDEX.md",
            fleet_dir / "state.md",
            project=project,
            wave=wave,
            outcome=outcome,
            reason=reason,
            fleet_root=fleet_dir.parent,
        )
    except ManifestError as exc:
        typer.echo(f"Manifest error: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    except MissionStateCorrupt as exc:
        typer.echo(f"Mission state corrupt: {exc}", err=True)
        raise typer.Exit(code=4) from exc

    typer.echo(json.dumps(out))


@app.command("status")
def cmd_status(
    mission_id: str = typer.Argument(..., help="Mission id (ab-XXXX) to query"),
) -> None:
    """Show progress for one mission."""
    fleet_dir = _resolve_mission(mission_id)
    if fleet_dir is None:
        typer.echo(f"Mission {mission_id} not found in {_fleet_root()}/", err=True)
        raise typer.Exit(code=2)

    state_path = fleet_dir / "state.md"
    try:
        state = read_state(state_path)
    except MissionStateCorrupt:
        backup = state_path.with_name(state_path.name + ".bak")
        typer.echo(f"Mission {mission_id}: state file unreadable; see {backup}")
        return

    manifest_path = fleet_dir / "00-INDEX.md"
    try:
        manifest = load_manifest(manifest_path)
    except ManifestError as exc:
        typer.echo(f"Mission {mission_id} status: {state.status} (manifest error: {exc})")
        return

    total_waves = len(manifest.waves)
    received_per_wave: dict[int, int] = {}
    for c in state.received_completes:
        wave_n = c.get("wave")
        if isinstance(wave_n, int):
            received_per_wave[wave_n] = received_per_wave.get(wave_n, 0) + 1

    typer.echo(f"Mission {mission_id} ({state.status})")
    typer.echo(f"  fleet dir: {fleet_dir}")
    for wave in manifest.waves:
        n = wave.wave
        sent = len(state.sent_msg_ids.get(f"wave_{n}", []))
        received = received_per_wave.get(n, 0)
        expected = len(wave.projects)
        typer.echo(
            f"  wave {n}/{total_waves}: {received}/{expected} complete, {sent} dispatched"
        )


@app.command("cancel")
def cmd_cancel(
    mission_id: str = typer.Argument(..., help="Mission id (ab-XXXX) to cancel"),
) -> None:
    """Mark mission cancelled and drop a sentinel for the running commander."""
    fleet_dir = _resolve_mission(mission_id)
    if fleet_dir is None:
        typer.echo(f"Mission {mission_id} not found in {_fleet_root()}/", err=True)
        raise typer.Exit(code=2)

    sentinel = fleet_dir / ".cancelled"
    try:
        sentinel.touch()
    except OSError as exc:
        typer.echo(f"Could not write cancel sentinel: {exc}", err=True)
        raise typer.Exit(code=4) from exc

    state_path = fleet_dir / "state.md"
    try:
        state = read_state(state_path)
    except MissionStateCorrupt as exc:
        typer.echo(f"Mission {mission_id} state unreadable: {exc}", err=True)
        raise typer.Exit(code=4) from exc

    # TERMINAL_STATUSES is single-sourced in _constants.py. Previously this
    # site hardcoded ("complete", "cancelled") and omitted "failed", which
    # made `fno megatron cancel <failed-mission>` throw MissionStateRegression
    # AFTER the sentinel was already touched. BUG-MT-002.
    if state.status not in TERMINAL_STATUSES:
        update_status(state_path, "cancelled")

    typer.echo(f"Mission {mission_id} cancelled. In-flight projects continue autonomously.")


@app.command("retro")
def cmd_retro(
    mission_id: str = typer.Argument(..., help="Mission id (ab-XXXX) to show retro for"),
) -> None:
    """Show a formatted retrospective for a complete mission."""
    fleet_dir = _resolve_mission(mission_id)
    if fleet_dir is None:
        typer.echo(f"Mission {mission_id} not found in {_fleet_root()}/", err=True)
        raise typer.Exit(code=2)

    state_path = fleet_dir / "state.md"
    try:
        state = read_state(state_path)
    except MissionStateCorrupt as exc:
        typer.echo(f"Mission state corrupt: {exc}", err=True)
        raise typer.Exit(code=4) from exc

    if state.status != "complete":
        typer.echo(
            f"Mission {mission_id} is in state {state.status}. "
            f"Retro is available only for complete missions.",
            err=True,
        )
        raise typer.Exit(code=4)

    from fno.megatron.artifact import mission_artifact_path

    artifact_path = mission_artifact_path(fleet_dir, mission_id)
    if not artifact_path.exists():
        typer.echo(
            f"Mission {mission_id} is complete but no artifact found at {artifact_path}. "
            f"(The artifact is written when status flips to complete; if state.md was "
            f"manually edited, the artifact may be absent.)",
            err=True,
        )
        raise typer.Exit(code=4)

    typer.echo(artifact_path.read_text(encoding="utf-8"))


@app.command("list")
def cmd_list() -> None:
    """List active fleet missions (running, paused, complete) in a 4-col table."""
    fleet = _fleet_root()
    if not fleet.exists():
        typer.echo("No fleet missions.")
        return

    rows: list[tuple[str, str, str, str]] = []
    for slug_dir in sorted(fleet.iterdir()):
        if not slug_dir.is_dir():
            continue
        state_path = slug_dir / "state.md"
        if not state_path.exists():
            continue
        try:
            state = read_state(state_path)
        except MissionStateCorrupt:
            try:
                mtime = state_path.stat().st_mtime
                ts = datetime.fromtimestamp(mtime, tz=timezone.utc).isoformat(timespec="seconds")
            except OSError:
                ts = "?"
            rows.append((slug_dir.name, "?", "corrupt", ts))
            continue
        rows.append(
            (
                slug_dir.name,
                state.mission_id,
                state.status,
                state.created_at or "?",
            )
        )

    if not rows:
        typer.echo("No fleet missions.")
        return

    header = ("slug", "mission_id", "status", "created_at")
    rows = [header, *rows]
    widths = [max(len(r[i]) for r in rows) for i in range(4)]
    for r in rows:
        typer.echo("  ".join(r[i].ljust(widths[i]) for i in range(4)))


@app.command("reconcile")
def cmd_reconcile(
    mission_id: str = typer.Argument(..., help="Mission id (ab-XXXX) to scan"),
    backfill: bool = typer.Option(
        False, "--backfill", help="Write missing completion JSONs for confirmed-merged PRs"
    ),
    as_json: bool = typer.Option(
        False,
        "--json", "-J",
        help="Emit structured JSON to stdout instead of markdown",
    ),
    verbose: bool = typer.Option(False, "--verbose", help="Show no-drift rows too"),
    pr_choice: Optional[int] = typer.Option(
        None,
        "--pr",
        help="Disambiguate ambiguous PRs by 1-indexed candidate position (with --backfill); ignored for single-candidate records",
    ),
) -> None:
    """Detect filesystem-vs-PR completion drift for a mission.

    Read-only by default; with --backfill writes missing completion JSONs
    for confirmed merged PRs. Refuses to clobber existing files. Never
    auto-resumes the mission.

    Exit codes:
      0 - no drift
      2 - unknown mission, manifest error, or invalid arguments
      3 - gh CLI unavailable at scan time (reserved for scan-level auth failure)
      4 - drift detected (read-only) or unresolved drift after --backfill
    """
    from fno.megatron.reconcile import (
        backfill_completion,
        render_drift_report,
        scan_drift,
    )

    if pr_choice is not None and pr_choice < 1:
        typer.echo(
            "--pr is 1-indexed; pass a positive integer (the Nth candidate).",
            err=True,
        )
        raise typer.Exit(code=2)

    fleet_dir = _resolve_mission(mission_id)
    if fleet_dir is None:
        msg = f"Mission {mission_id} not found in {_fleet_root()}/"
        if as_json:
            typer.echo(json.dumps({"error": msg}))
        else:
            typer.echo(msg, err=True)
        raise typer.Exit(code=2)

    manifest_path = fleet_dir / "00-INDEX.md"
    state_path = fleet_dir / "state.md"

    try:
        manifest = load_manifest(manifest_path)
    except ManifestError as exc:
        typer.echo(f"Manifest error: {exc}", err=True)
        raise typer.Exit(code=2) from exc

    try:
        state = read_state(state_path)
    except MissionStateCorrupt as exc:
        typer.echo(f"Mission state corrupt: {exc}", err=True)
        raise typer.Exit(code=4) from exc

    mission_slug = state.slug or fleet_dir.name

    # Scan-level auth failure (gh CLI missing) -> exit 3. Per-record query
    # failures surface as record.state="query-failed" inside the report.
    import shutil as _shutil

    if _shutil.which("gh") is None:
        typer.echo(
            "reconcile: 'gh' CLI not found on PATH; cannot query PR state",
            err=True,
        )
        raise typer.Exit(code=3)

    report = scan_drift(manifest, fleet_dir, mission_slug)

    if backfill:
        multi = [
            r
            for r in report.drift
            if not r.completion_exists and len(r.pr_candidates) > 1
        ]
        if pr_choice is not None and len(multi) > 1:
            typer.echo(
                f"--pr applied across {len(multi)} multi-candidate records; "
                f"rerun per-record (with one --backfill at a time) if "
                f"disambiguation differs.",
                err=True,
            )

        unresolved = 0
        for record in report.drift:
            if record.completion_exists:
                continue
            if record.state in ("missing-pr-merged", "ambiguous"):
                if len(record.pr_candidates) > 1 and pr_choice is None:
                    record.backfill_attempted = True
                    record.backfill_skipped_reason = (
                        f"{len(record.pr_candidates)} candidates; pass --pr N "
                        f"to disambiguate"
                    )
                    unresolved += 1
                    continue
                # Only honor --pr for records that actually need disambiguation.
                # A single-candidate record always uses index 0 regardless of
                # what --pr was set to (codex P2 / Gemini PR #262).
                if pr_choice is not None and len(record.pr_candidates) > 1:
                    pr_idx = pr_choice - 1
                else:
                    pr_idx = 0
                if not backfill_completion(
                    record, mission_id=mission_id, pr_choice_index=pr_idx
                ):
                    unresolved += 1
            else:
                unresolved += 1

        typer.echo(render_drift_report(report, as_json=as_json, verbose=verbose))
        if unresolved > 0:
            raise typer.Exit(code=4)
        return

    typer.echo(render_drift_report(report, as_json=as_json, verbose=verbose))
    if report.has_drift:
        raise typer.Exit(code=4)
