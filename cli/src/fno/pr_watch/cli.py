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
import signal
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


def _emit_event(
    event_type: str, data: dict[str, Any], *, events_path: Optional[Path] = None
) -> bool:
    """Append a canonical event envelope to events.jsonl.

    Uses fno.events._build + fno.events.append_event (the same path the
    ``fno event emit`` CLI verb uses internally).  On failure, logs a warning
    instead of silently passing so the failure is observable.

    When no explicit ``events_path`` is given, defaults to
    ``state_dir()/events.jsonl`` -- the same global path that the status
    command's watermark scan reads from and that the watermark
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
            return False
    try:
        from fno.events import _build, append_event
        event = _build(event_type, "daemon", data)
        append_event(event, events_path)
        return True
    except Exception as exc:
        log.warning("pr-watch: emit %s failed: %s", event_type, exc)
        return False


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


def _catchup_roots() -> list[Path]:
    """Distinct project roots the canonical-sync catch-up should sweep.

    launchd starts this daemon in ``/`` with no WorkingDirectory, so there is no
    ambient project to read config from. The roots come from the backlog graph,
    the same source the dispatch pass scopes its own per-repo config reads to.
    """
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        entries = read_graph(graph_json())
    except Exception as exc:  # noqa: BLE001 - no graph means nothing to sweep
        log.warning("pr-watch: could not read graph for catch-up roots: %s", exc)
        return []
    roots: dict[str, Path] = {}
    for node in entries:
        cwd = node.get("cwd")
        if cwd and str(cwd) not in roots:
            roots[str(cwd)] = Path(cwd)
    return [p for p in roots.values() if p.is_dir()]


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

# EX_TEMPFAIL: launchd logs the non-zero exit but does not respawn a
# StartInterval job early, so a timed-out tick surfaces without suppressing
# the successor it was bounded to protect.
_TICK_TIMEOUT_EXIT = 75

_ENV_TICK_TIMEOUT = "FNO_PR_WATCH_TICK_TIMEOUT"


class TickDeadlineExceeded(BaseException):
    """The tick's wall-clock deadline fired; the phase marker names where.

    BaseException on purpose: every broad `except Exception` seam in the tick
    path (the sweep, recovery, catch-up) exists to degrade one leg without
    stopping the others, and the deadline is the one signal that must stop
    everything. The alarm is one-shot, so a seam that swallowed it would leave
    the rest of the tick unbounded - the exact stall class this deadline ends.
    """


def _on_deadline(signum, frame) -> None:  # noqa: ARG001 - signal handler signature
    raise TickDeadlineExceeded()


def _resolve_tick_deadline(cfg) -> int:
    """Env seam first, then config, then 0.8x the interval (min 60s).

    Config and derived values are clamped BELOW interval_seconds: launchd
    never runs a StartInterval job concurrently, so a deadline at or above
    the interval would let an overrun suppress the successor tick - the exact
    failure mode this ceiling exists to prevent. The env seam stays
    unclamped; it is an operator escape hatch, not a durable setting.
    """
    env = (os.environ.get(_ENV_TICK_TIMEOUT) or "").strip()
    if env.isdigit() and int(env) > 0:
        return int(env)
    ceiling = max(1, int(cfg.interval_seconds) - 5)
    if cfg.tick_timeout_seconds:
        return min(int(cfg.tick_timeout_seconds), ceiling)
    derived = max(60, int(cfg.interval_seconds * 0.8))
    return min(derived, ceiling)


def _tick_outcome(result, tick_failed: Optional[str], timed_out: bool) -> str:
    """Map one tick run to its end-record outcome (AC table in the plan)."""
    if timed_out:
        return "timeout"
    if tick_failed is not None:
        return "error"
    if result is None:
        return "error"
    if result.disabled:
        return "disabled"
    if result.lock_held:
        return "lock_held"
    if getattr(result, "quota_skip", False):
        return "quota_skip"
    if getattr(result, "sweep_failures", 0):
        return "degraded"
    return "ok"


@cli.command()
def tick() -> None:
    """Poll open-PR backlog nodes and fire /fno:pr check or /fno:pr merged.

    This is the command the LaunchAgent's ProgramArguments points at.
    It builds the real adapters (claims, emit, reviewers_for, etc.) and
    calls tick() from fno.pr_watch._dispatch.
    """
    import time

    from fno.config_cli import post_merge_readiness
    from fno.pr_watch._dispatch import current_tick_phase, set_tick_phase
    from fno.pr_watch._dispatch import tick as _tick

    started = time.monotonic()
    # Entry is recorded before anything that can hang: settings load, imports,
    # and the graph read all precede any other record, so a tick that dies
    # mid-bootstrap is still attributable (AC8). This is NOT the liveness
    # watermark: only a completed sweep mints pr_watch_tick (AC9).
    set_tick_phase("entry")
    _emit_event(
        "pr_watch_tick_attempt",
        {"pid": os.getpid(), "phase": "entry"},
    )

    outcome = "error"
    result = None
    tick_failed = None
    timed_out = False
    settings = None

    try:
        set_tick_phase("settings")
        settings = load_settings()
        cfg = settings.pr_watch

        # x-aaaf wave 3: the master panic switch outranks pr_watch's own gate too.
        tick_enabled = cfg.enabled and settings.autonomy.enabled

        deadline = _resolve_tick_deadline(cfg)
        # SIGALRM, not a thread timer: the observed 22-minute 0%-CPU hang sat
        # in fcntl.flock(LOCK_EX) with no timeout (graph/store.py), and only a
        # signal can interrupt a main thread blocked in a syscall - a timer
        # thread would watch the deadline pass and then do nothing. alarm(0)
        # in the finally cancels an untriggered deadline.
        try:
            signal.signal(signal.SIGALRM, _on_deadline)
            signal.alarm(deadline)
        except ValueError:
            # Not the main thread (tests embedding the command): no alarm
            # available, run unbounded like before.
            log.debug("pr-watch: SIGALRM unavailable outside main thread")

        set_tick_phase("sweep")
        # A dead tick must not kill the legs below. The receipt contract makes
        # _tick raise on a failed emission even though state is already persisted,
        # so a broken events path would otherwise crash-loop recovery and sync
        # catch-up, which ride this same launchd cadence. Fail the exit code at
        # the end instead, mirroring how those legs wrap their own failures.
        try:
            result = _tick(
                claim=ClaimAdapter(),
                emit=_emit_event,
                reviewers_for=_reviewers_for,
                notify=lambda message, **_kw: _notify_parked(message),
                post_merge_readiness_fn=post_merge_readiness,
                now_iso=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                max_age_days=cfg.max_age_days,
                max_retries=cfg.retries,
                graphql_min_remaining=cfg.graphql_min_remaining,
                enabled=tick_enabled,
            )
        except TickDeadlineExceeded:
            raise
        except Exception as exc:  # noqa: BLE001 - a dead events path must not stop recovery
            tick_failed = str(exc)
            log.warning("pr-watch: tick failed: %s", exc)
            typer.echo(f"pr-watch tick: failed: {exc}", err=True)
            result = None

        if result is not None:
            if result.disabled:
                reason = "config.autonomy.enabled" if not settings.autonomy.enabled else "config.pr_watch.enabled"
                typer.echo(f"pr-watch tick: {reason} is false - skipped")
            elif result.lock_held:
                typer.echo(f"pr-watch tick: {result.lock_holder} - skipped")
            elif result.quota_skip:
                reset = f", resets {result.quota_reset}" if result.quota_reset else ""
                typer.echo(
                    f"pr-watch tick: graphql remaining {result.quota_remaining} below floor"
                    f" - dispatch pass skipped{reset}"
                )
            elif result.sweep_failures:
                typer.echo(
                    f"pr-watch tick: degraded: {result.sweep_failures} sweep failure(s)"
                )
            else:
                typer.echo(
                    f"pr-watch tick: open_prs={result.open_prs} acted={result.acted} skipped={result.skipped}"
                )

        # Session recovery rides this same launchd cadence: a sweep over
        # footnote-launched bg /target sessions that rotates providers on swap-class
        # deaths and surfaces finished-but-lingering sessions to close. The held
        # socket nudge was removed (a bypass recipient holds it by design), so the
        # sweep no longer resumes idle-but-incomplete sessions. Gated by
        # config.recovery.enabled and wrapped non-fatally so a recovery failure
        # never breaks the PR-watch tick. The master switch (x-aaaf wave 3) outranks
        # this gate too - a recovery respawn is exactly the "session starts itself"
        # behavior the panic switch exists to stop.
        set_tick_phase("recovery")
        if settings.recovery.enabled and settings.autonomy.enabled:
            try:
                from fno.recovery import run_recovery_sweep

                def emit_recovery(event_type: str, data: dict) -> None:
                    _emit_event(event_type, data)

                n = run_recovery_sweep(settings.recovery, emit=emit_recovery)
                typer.echo(f"recovery sweep: candidates={n}")
            except Exception as exc:  # noqa: BLE001 - never let recovery break pr-watch
                log.warning("pr-watch: recovery sweep failed: %s", exc)

        # Canonical-sync catch-up. The dispatch above is event-time-only:
        # it acts on merges it DETECTS, so a merge that landed while the daemon was
        # wedged is never synced by it. This leg is keyed on outcome instead - it
        # asks whether recent merges have markers, not whether we saw them happen.
        # Wrapped exactly like the recovery sweep: a catch-up failure logs and never
        # breaks the tick. auto_run gating lives inside run_sync_catchup.
        # Deferred after a quota skip: this leg's gh pr list/view calls spend the
        # same shared GraphQL pool the skip just refused to drain, so running it
        # would stall for each timeout against the exact budget it protected.
        set_tick_phase("catchup")
        quota_skipped = result is not None and bool(getattr(result, "quota_skip", False))
        if not quota_skipped:
            try:
                from fno.pr._sync_canonical import run_sync_catchup

                for root in _catchup_roots():
                    try:
                        res = run_sync_catchup(
                            settings=load_settings_for_repo(root), canonical_root=root
                        )
                    except Exception as exc:  # noqa: BLE001 - one bad repo never stops the rest
                        log.warning("pr-watch: sync catch-up failed for %s: %s", root, exc)
                        continue
                    if res.outcome == "disabled":
                        continue
                    typer.echo(
                        f"sync catch-up [{root.name}]: {res.outcome}"
                        + (f" ({res.detail})" if res.detail else "")
                    )
                    # Detected AND unresolved. Keying on a failed sync alone would alarm
                    # on a merge from two minutes ago whose retry is seconds away, and
                    # stay silent on a canonical proven behind with every marker present
                    # - the state where there is nothing to sweep and the markers lie.
                    if res.stale and res.outcome != "synced":
                        typer.echo(
                            f"ALARM: {root.name} canonical sync is stale and the catch-up "
                            f"did not resolve it ({res.detail}). That checkout and its "
                            f"installed tooling are behind; sync it by hand.",
                            err=True,
                        )
                        _notify_parked(f"canonical sync stale: {root.name} ({res.outcome})")
            except Exception as exc:  # noqa: BLE001 - never let catch-up break pr-watch
                log.warning("pr-watch: sync catch-up failed: %s", exc)
    except TickDeadlineExceeded:
        # The deadline fired somewhere above; phase names where. Recovery and
        # catch-up are skipped on purpose: the process has already overrun the
        # interval and launchd's next tick must not be suppressed further.
        timed_out = True
        typer.echo(
            f"pr-watch tick: deadline exceeded in phase {current_tick_phase()} - aborted",
            err=True,
        )
    finally:
        try:
            signal.alarm(0)
        except ValueError:
            pass
        outcome = _tick_outcome(result, tick_failed, timed_out)
        end_data: dict[str, Any] = {
            "outcome": outcome,
            "duration_s": round(time.monotonic() - started, 3),
            "phase": current_tick_phase(),
            "pid": os.getpid(),
        }
        if result is not None:
            end_data["sweep_failures"] = getattr(result, "sweep_failures", 0)
            if getattr(result, "quota_skip", False):
                end_data["quota_remaining"] = result.quota_remaining
                end_data["quota_reset"] = result.quota_reset
            if getattr(result, "quota_unknown", False):
                end_data["quota_unknown"] = True
        # The end record always fires - including on timeout and error - so the
        # attempt/end pair brackets every invocation; only outcome=ok/degraded
        # corresponds to a pr_watch_tick (the liveness watermark) having fired.
        _emit_event("pr_watch_tick_end", end_data)

    if timed_out:
        raise typer.Exit(code=_TICK_TIMEOUT_EXIT)
    if tick_failed is not None:
        raise typer.Exit(code=1)


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
    except claims.CLAIM_UNAVAILABLE:
        # Someone else is on it, not a reason to abort this SessionStart
        # hook with a traceback.
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
