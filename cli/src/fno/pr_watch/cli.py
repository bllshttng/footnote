"""fno pr-watch CLI surface.

Four verbs:
  tick      - the launchd entry; builds real adapters and calls tick()
  install   - render + gate-confirm + write global LaunchAgent plist
  uninstall - unload (best-effort) + remove plist; preserve watermark store
  status    - report loaded/unloaded, last tick, open-PR count, parked PRs

Logic lives in _install.py; this module stays thin (Typer glue only).
"""
from __future__ import annotations

import json
import logging
import os
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import typer

log = logging.getLogger(__name__)

cli = typer.Typer(
    name="pr-watch",
    help="PR-state watcher: auto-fire /pr check + /pr merged for open-PR backlog nodes.",
    no_args_is_help=True,
)

_LAUNCH_AGENTS_DIR = Path.home() / "Library" / "LaunchAgents"


def _resolve_fno_binary() -> str:
    """Return the absolute path to the fno binary.

    Tries shutil.which first; falls back to the console-script alongside
    the current interpreter (handles ``uv run --project cli fno-py`` dev use).
    Resolves `fno-py` (the console script); the Rust mux binary owns `fno`.
    """
    found = shutil.which("fno-py")
    if found:
        return os.path.abspath(found)
    # Fallback: the entry-point next to the running Python interpreter
    candidate = Path(sys.executable).parent / "fno-py"
    if candidate.exists():
        return str(candidate)
    return "fno-py"  # last resort: bare name (launchd may still find it via PATH)


# ---------------------------------------------------------------------------
# Module-level adapter callables (extracted for testability)
# ---------------------------------------------------------------------------


def _emit_event(event_type: str, data: dict[str, Any], *, events_path: Optional[Path] = None) -> None:
    """Append a canonical event envelope to events.jsonl.

    Uses fno.events._build + fno.events.append_event (the same path the
    ``fno event emit`` CLI verb uses internally).  On failure, logs a warning
    instead of silently passing so the failure is observable.

    When no explicit ``events_path`` is given, defaults to
    ``state_dir()/events.jsonl`` -- the same global path that
    ``_last_tick_ts`` (status command) reads from and that the watermark
    store anchors to.  This makes the daemon cwd-independent: launchd
    starts the daemon in ``/`` with no WorkingDirectory, so any cwd-relative
    path (e.g. ``Path(".fno/events.jsonl")``) would be silently lost.
    """
    if events_path is None:
        try:
            from fno.paths import state_dir
            events_path = state_dir() / "events.jsonl"
        except Exception as exc:
            log.warning("pr-watch: could not resolve state_dir for events path: %s", exc)
            return
    try:
        from fno.events import _build, append_event
        event = _build(event_type, "daemon", data)
        append_event(event, events_path)
    except Exception as exc:
        log.warning("pr-watch: emit %s failed: %s", event_type, exc)


def _notify_parked(message: str) -> None:
    """Send an OS notification for a parked PR.

    Calls send_notification with (title, message) -- two positional args.
    On failure, logs a warning instead of silently passing.
    """
    try:
        send_notification("pr-watch", message)
    except Exception as exc:
        log.warning("pr-watch: notify failed: %s", exc)


def _reviewers_for(repo_dir: Path) -> list[str]:
    """Return the configured external reviewers for a given repo root.

    Loads settings scoped to ``repo_dir`` so each candidate PR uses its own
    repo's ``config.review.github_apps`` (aka the legacy ``required_bots``)
    rather than the daemon's cwd.  Falls back to [] when none are configured
    (review-dispatch skipped; merge-dispatch still works).  Logs a warning on
    error so a broken settings.yaml is visible rather than silently disabling
    review-dispatch.
    """
    try:
        s = load_settings_for_repo(repo_dir)
        bots = s.review.github_apps
        return list(bots) if bots else []
    except Exception as exc:
        log.warning(
            "pr-watch: reviewer resolution failed (%s); review-dispatch disabled this tick",
            exc,
        )
        return []


class ClaimAdapter:
    """Thin adapter that maps the tick() claim protocol to fno.claims."""

    def acquire_tick_lock(self, key: str, holder: str) -> None:
        from fno.claims import acquire_claim
        acquire_claim(key, holder=holder)

    def release_tick_lock(self, key: str, holder: str) -> None:
        try:
            from fno.claims import release_claim
            release_claim(key, holder=holder)
        except Exception:
            pass

    def acquire_pr_lock(self, key: str, holder: str) -> None:
        from fno.claims import acquire_claim
        acquire_claim(key, holder=holder)

    def release_pr_lock(self, key: str, holder: str) -> None:
        try:
            from fno.claims import release_claim
            release_claim(key, holder=holder)
        except Exception:
            pass

    def is_node_live(self, node_id: str) -> bool:
        """Return True when the node has a live session claim.

        Fails SAFE: on exception, returns True (treat as live) to avoid
        double-dispatch onto a node a live /target session owns.
        """
        try:
            info = claim_status(f"node:{node_id}")
            # live OR suspect (x-ba4b): a suspect claim (TTL-unexpired, dead pid)
            # is a respawned worker's slot - treat as occupied, never re-dispatch.
            return info.get("state") in ("live", "suspect")
        except Exception as exc:
            log.warning(
                "pr-watch: claim_status failed for node %s (%s); treating as live (fail-safe)",
                node_id,
                exc,
            )
            return True


# ---------------------------------------------------------------------------
# Module-level imports used by adapters (importable at test-patch time)
# ---------------------------------------------------------------------------

from fno.claims.core import claim_status  # noqa: E402
from fno.config import load_settings, load_settings_for_repo  # noqa: E402
from fno.notify._impl import send_notification  # noqa: E402


# ---------------------------------------------------------------------------
# tick
# ---------------------------------------------------------------------------


@cli.command()
def tick() -> None:
    """Poll open-PR backlog nodes and fire /fno:pr check or /fno:pr merged.

    This is the command the LaunchAgent's ProgramArguments points at.
    It builds the real adapters (claims, emit, reviewers_for, etc.) and
    calls tick() from fno.pr_watch._dispatch.
    """
    from fno.config_cli import post_merge_readiness
    from fno.pr_watch._dispatch import tick as _tick

    settings = load_settings()
    cfg = settings.pr_watch

    result = _tick(
        claim=ClaimAdapter(),
        emit=_emit_event,
        reviewers_for=_reviewers_for,
        notify=lambda message, **_kw: _notify_parked(message),
        post_merge_readiness_fn=post_merge_readiness,
        now_iso=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        max_age_days=cfg.max_age_days,
        max_retries=cfg.retries,
    )

    if result.lock_held:
        typer.echo(f"pr-watch tick: lock held by {result.lock_holder} - skipped")
    else:
        typer.echo(
            f"pr-watch tick: open_prs={result.open_prs} acted={result.acted} skipped={result.skipped}"
        )

    # Session auto-recovery (x-f47c) rides this same launchd cadence: a sweep
    # that resumes footnote-launched bg sessions which went idle-but-incomplete
    # after an abnormal turn termination. Gated by config.recovery.enabled and
    # wrapped non-fatally so a recovery failure never breaks the PR-watch tick.
    if settings.recovery.enabled:
        try:
            from fno.recovery import run_recovery_sweep

            n = run_recovery_sweep(settings.recovery, emit=_emit_event)
            typer.echo(f"recovery sweep: candidates={n}")
        except Exception as exc:  # noqa: BLE001 - never let recovery break pr-watch
            log.warning("pr-watch: recovery sweep failed: %s", exc)


# ---------------------------------------------------------------------------
# install
# ---------------------------------------------------------------------------


@cli.command()
def install(
    dry_run: bool = typer.Option(False, "-N", "--dry-run", help="Print plist; write nothing."),
    interval: int = typer.Option(0, "--interval", help="Poll interval in seconds (0 = use config)."),
    model: str = typer.Option("", "--model", help="Model for headless fires (empty = use config)."),
    no_activate: bool = typer.Option(
        False,
        "--no-activate",
        help="Write the plist but do NOT launchctl load it (packaging/CI escape).",
    ),
) -> None:
    """Render and install the global PR-state watcher LaunchAgent, then load it.

    Prints the full plist before writing.  Requires explicit confirmation
    before writing to ~/Library/LaunchAgents/, then runs ``launchctl load`` so
    enabled means running (x-e106).  Pass ``--no-activate`` to write only.
    """
    from fno.pr_watch import _install as m

    settings = load_settings()
    cfg = settings.pr_watch

    _interval = interval if interval > 0 else cfg.interval_seconds

    m.install(
        launch_agents_dir=_LAUNCH_AGENTS_DIR,
        fno_binary=_resolve_fno_binary(),
        install_path=os.environ.get("PATH", "/usr/bin:/bin"),
        interval=_interval,
        dry_run=dry_run,
        activate=not no_activate,
    )


@cli.command()
def refresh() -> None:
    """Re-render the plist onto the current binary and bounce the watcher.

    Non-interactive, no confirm prompt: this is the tail of ``fno update`` (so
    an update leaves an enabled watcher running the freshly-installed binary),
    and is safe to run by hand. A no-op when ``pr_watch.enabled`` is false, so
    an install that does not use the watcher gets nothing. Never fails loud:
    the update chain calls it best-effort and a refresh failure must not fail
    the update.
    """
    from fno.pr_watch import _install as m

    settings = load_settings()
    if not settings.pr_watch.enabled:
        typer.echo("pr-watch: disabled; nothing to refresh.")
        return

    msg, _rc = m.refresh_watcher(
        launch_agents_dir=_LAUNCH_AGENTS_DIR,
        fno_binary=_resolve_fno_binary(),
        install_path=os.environ.get("PATH", "/usr/bin:/bin"),
        interval=settings.pr_watch.interval_seconds,
    )
    typer.echo(f"pr-watch refresh: {msg}")


# Single-flight window for the SessionStart self-heal: long enough to cover the
# render + bounce round-trip, short enough that a crashed heal recovers soon.
_HEAL_TTL_MS = 5 * 60 * 1000


@cli.command()
def heal() -> None:
    """Revive a previously-enabled-but-dead watcher (idempotent, race-guarded).

    The SessionStart self-heal entrypoint: fired when the liveness verdict is
    ``dead``. Acts only when ``pr_watch.enabled`` is true, so a never-enabled
    watcher is never auto-installed; a claim single-flights concurrent
    SessionStarts so the reinstall happens at most once per window. The heal
    itself is ``refresh_watcher`` (re-render plist + bounce), which cures both
    an unloaded agent and the wedged-job state a plain ``launchctl load``
    cannot fix.
    """
    from fno import claims
    from fno.claims.io import global_claims_root
    from fno.pr_watch import _install as m

    settings = load_settings()
    if not settings.pr_watch.enabled:
        typer.echo("pr-watch heal: disabled; nothing to heal")
        return

    holder = f"pr-watch-heal:{os.getpid()}"
    heal_root = global_claims_root()
    try:
        claims.acquire_claim(
            "pr-watch:heal", holder, ttl_ms=_HEAL_TTL_MS,
            reason="pr-watch SessionStart self-heal", root=heal_root,
        )
    except claims.ClaimHeldByOther:
        typer.echo("pr-watch heal: another session is healing; skipped")
        return
    try:
        msg, rc = m.refresh_watcher(
            launch_agents_dir=_LAUNCH_AGENTS_DIR,
            fno_binary=_resolve_fno_binary(),
            install_path=os.environ.get("PATH", "/usr/bin:/bin"),
            interval=settings.pr_watch.interval_seconds,
        )
        typer.echo(f"pr-watch heal: {msg}")
        if rc != 0:
            raise typer.Exit(1)
    finally:
        try:
            claims.release_claim("pr-watch:heal", holder, root=heal_root)
        except Exception:  # noqa: BLE001 - TTL-bounded; a failed release self-recovers
            pass


# ---------------------------------------------------------------------------
# Activation coupling entrypoints (called by `fno config set pr_watch.enabled`)
# ---------------------------------------------------------------------------


def ensure_watcher_activated() -> str:
    """Install + load the global watcher if absent (idempotent, non-interactive).

    The config-set hook path: it must never prompt (the interactive install
    confirm would wedge a headless `fno config set`).  Returns the outcome
    string from ``_install.ensure_activated``.
    """
    from fno.pr_watch import _install as m

    return m.ensure_activated(
        launch_agents_dir=_LAUNCH_AGENTS_DIR,
        fno_binary=_resolve_fno_binary(),
        install_path=os.environ.get("PATH", "/usr/bin:/bin"),
        interval=load_settings().pr_watch.interval_seconds,
    )


def deactivate_watcher() -> str:
    """Unload the watcher (keep the plist) when pr_watch.enabled is set false."""
    from fno.pr_watch import _install as m

    return m.unload_only(launch_agents_dir=_LAUNCH_AGENTS_DIR)


# ---------------------------------------------------------------------------
# uninstall
# ---------------------------------------------------------------------------


@cli.command()
def uninstall() -> None:
    """Unload (best-effort) and remove the global PR-state watcher LaunchAgent.

    Preserves ~/.fno/pr-watcher-state.json so a reinstall does not re-fire
    previously handled PRs.
    """
    from fno.pr_watch import _install as m

    m.uninstall(launch_agents_dir=_LAUNCH_AGENTS_DIR)


# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------


@cli.command()
def status(
    json_out: bool = typer.Option(
        False,
        "--json", "-J",
        help="Emit the liveness verdict as one JSON object (for hooks/scripts).",
    ),
) -> None:
    """Report watcher status: loaded, last tick time, open-PR count, parked PRs."""
    from fno.pr_watch import _install as m

    if json_out:
        typer.echo(json.dumps(m.liveness_report_live()))
        return
    m.status(launch_agents_dir=_LAUNCH_AGENTS_DIR)
