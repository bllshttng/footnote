"""fno pr-watch CLI surface.

Four verbs:
  tick      - the launchd entry; builds real adapters and calls tick()
  install   - render + gate-confirm + write global LaunchAgent plist
  uninstall - unload (best-effort) + remove plist; preserve watermark store
  status    - report loaded/unloaded, last tick, open-PR count, parked PRs

Logic lives in _install.py; this module stays thin (Typer glue only).
"""
from __future__ import annotations

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
    repo's ``config.review.required_bots`` rather than the daemon's cwd.
    Falls back to [] when none are configured (review-dispatch skipped;
    merge-dispatch still works).  Logs a warning on error so a broken
    settings.yaml is visible rather than silently disabling review-dispatch.
    """
    try:
        s = load_settings_for_repo(repo_dir)
        bots = s.config.review.required_bots
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
            return info.get("state") == "live"
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
    cfg = settings.config.pr_watch

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

    typer.echo(
        f"pr-watch tick: open_prs={result.open_prs} acted={result.acted} skipped={result.skipped}"
    )

    # Session auto-recovery (x-f47c) rides this same launchd cadence: a sweep
    # that resumes footnote-launched bg sessions which went idle-but-incomplete
    # after an abnormal turn termination. Gated by config.recovery.enabled and
    # wrapped non-fatally so a recovery failure never breaks the PR-watch tick.
    if settings.config.recovery.enabled:
        try:
            from fno.recovery import run_recovery_sweep

            n = run_recovery_sweep(settings.config.recovery, emit=_emit_event)
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
) -> None:
    """Render and (optionally) install the global PR-state watcher LaunchAgent.

    Prints the full plist before writing.  Requires explicit confirmation
    before writing to ~/Library/LaunchAgents/.  Does NOT run launchctl load
    (human gate: you review the plist and load it yourself).
    """
    from fno.pr_watch import _install as m

    settings = load_settings()
    cfg = settings.config.pr_watch

    _interval = interval if interval > 0 else cfg.interval_seconds

    m.install(
        launch_agents_dir=_LAUNCH_AGENTS_DIR,
        fno_binary=_resolve_fno_binary(),
        install_path=os.environ.get("PATH", "/usr/bin:/bin"),
        interval=_interval,
        dry_run=dry_run,
    )


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
def status() -> None:
    """Report watcher status: loaded, last tick time, open-PR count, parked PRs."""
    from fno.pr_watch import _install as m

    m.status(launch_agents_dir=_LAUNCH_AGENTS_DIR)
