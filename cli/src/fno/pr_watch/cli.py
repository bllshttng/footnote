"""fno do pr watch CLI surface.

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
from typing import Any, Callable, Optional

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
    ``fno doctor event emit`` CLI verb uses internally).  On failure, logs a warning
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


def _emit_for_sweep(event_type: str, data: dict[str, Any]) -> None:
    """run_sweep's emit contract drops the write receipt _emit_event returns."""
    _emit_event(event_type, data)


def _bounce_sender(window_s: float = 15.0) -> str:
    """The bounce that just sent this SIGTERM, from its receipt; else unrecorded.

    A hand ``kill -TERM``, or a sidecar older than ``window_s`` from an earlier
    bounce, reads ``unrecorded`` rather than blaming the wrong cure. Never raises.
    """
    try:
        import time
        from fno.paths import state_dir
        from fno.pr_watch._install import _BOUNCE_SIDECAR

        raw = json.loads((state_dir() / _BOUNCE_SIDECAR).read_text(encoding="utf-8"))
        if time.time() - float(raw["ts"]) <= window_s:
            return f"{raw['caller']} pid {raw['pid']} via {raw.get('parent', '')}".rstrip()
    except Exception:  # noqa: BLE001 - evidence, never a gate
        pass
    return "unrecorded"


#: The launchd label every phase in this tick rides on; each tick row names it.
_PR_WATCH_SCHEDULER = "launchd:sh.fno.pr-watcher"


def _emit_tick_row(arm: str, *, interval_s: int, acted: int = 0,
                   skip_reason: Optional[str] = None, detail: Optional[str] = None) -> None:
    """One arm row per phase outcome (never raises); rides ``_emit_event`` so
    the ``_no_global_tick_events`` fixture captures it."""
    data: dict[str, Any] = {"arm": arm, "scheduler": _PR_WATCH_SCHEDULER,
                            "acted": acted, "interval_s": int(interval_s)}
    if skip_reason is not None:
        data["skip_reason"] = skip_reason
    if detail is not None:
        # Same bound as fno.control_plane.emit_tick: a refusal survives whole,
        # a pathological stderr cannot write a giant journal row.
        data["detail"] = detail[:4000]
    _emit_event("control_plane_tick", data)


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
    ambient project to read config from. The roots come from every sidecar's
    cwd, regardless of the node's tracker state - a project whose backlog is
    all done/closed must still get swept, so this is ``load_all()`` (one scan,
    every id), never ``list_open()`` plus a per-id ``load()`` loop.
    """
    try:
        from fno.tracker import sidecar as sidecar_store

        sidecars = sidecar_store.load_all()
    except Exception as exc:  # noqa: BLE001 - no sidecar store means nothing to sweep
        log.warning("pr-watch: could not read sidecars for catch-up roots: %s", exc)
        return []
    roots: dict[str, Path] = {}
    for sc in sidecars.values():
        cwd = sc.cwd
        if cwd and str(cwd) not in roots:
            roots[str(cwd)] = Path(cwd)
    return [p for p in roots.values() if p.is_dir()]


def _run_notify_watch_phase(
    roots: "Optional[list[Path]]" = None,
    *,
    timeout_s: Optional[float] = None,
) -> None:
    """Run the Rust notify_watch arm and turn its receipt into the tick row.

    The arm lives in fno-agents (``notify-watch``); the sampler, the signal
    store and the ``[notify]`` config are all read in Rust, so this phase is
    only spawn, parse and emit. The subprocess runs inside the first catch-up
    root: launchd starts this daemon in ``/``, where a board read would read
    an empty world. An absent binary, a non-zero run and an unparseable
    receipt all land as ``notify_failed`` - a dead notice lane never raises
    out of the tick. ``roots`` rides the tick's one-scan memo; the two tick
    phases are named apart so a cut says which half stalled. The subprocess
    bound comes from the phase slice (minus a 2s reserve) when the caller
    passes it, else from the armed phase deadline, and only falls back to a
    literal when no phase is armed at all.
    """
    from fno.pr_watch._dispatch import phase_seconds_left, set_tick_phase

    set_tick_phase("notify_watch")
    if roots is None:
        set_tick_phase("notify_watch:roots")
        roots = _catchup_roots()
    try:
        import subprocess

        from fno.rust_binary import resolve_binary

        binary = resolve_binary()
        if binary is None:
            _emit_tick_row("notify_watch", interval_s=300,
                           skip_reason="notify_failed", detail="rust binary absent")
            return
        set_tick_phase("notify_watch:arm")
        argv = [str(binary), "notify-watch", "--json"]
        for root in roots:
            argv += ["--root", str(root)]
        if timeout_s is None:
            left = phase_seconds_left()
            timeout_s = max(1.0, left - 2.0) if left is not None else 240.0
        proc = subprocess.run(
            argv, capture_output=True, text=True, check=False, timeout=timeout_s,
            cwd=str(roots[0]) if roots else None,
        )
        payload = json.loads(proc.stdout or "{}")
        _emit_tick_row("notify_watch", interval_s=300,
                       acted=int(payload.get("acted") or 0),
                       skip_reason=payload.get("skip_reason"),
                       detail=(payload.get("detail") or "")[:200])
    except Exception as exc:  # noqa: BLE001 - never let a notice break the tick
        log.warning("pr-watch: notify_watch phase failed: %s", exc)
        _emit_tick_row("notify_watch", interval_s=300,
                       skip_reason="notify_failed", detail=str(exc)[:200])


def _run_evals_arm_phase(settings: Any, *, seconds_left_fn) -> None:
    """The eval bank's demand leg: guards and the receipt parse here;
    the due read, gate, detached run and journal live in native evals-arm.
    Every failure lands as ``arm_failed``, never out of the tick."""
    evals_cfg = getattr(settings, "evals", None)
    days = int(getattr(evals_cfg, "schedule_days", 0) or 0)
    interval_s = days * 86400

    def row(skip: Optional[str], detail: str, acted: int = 0) -> None:
        _emit_tick_row("evals", interval_s=interval_s, acted=acted,
                       skip_reason=skip, detail=detail[:400])

    if days <= 0:
        row("evals_off", "evals.schedule_days 0")
        return
    try:
        from fno.config import autonomy_master_enabled
        armed = autonomy_master_enabled()
    except Exception:  # noqa: BLE001 - an unreadable master switch reads off
        armed = False
    if not armed:
        row("autonomy_off", "config.autonomy.enabled is false")
        return
    try:
        import subprocess

        from fno.evals.report import evals_health_summary
        from fno.paths import evals_history, state_dir
        from fno.rust_binary import resolve_binary

        binary = resolve_binary()
        if binary is None:
            raise RuntimeError("fno-agents binary not found")
        argv = [
            str(binary), "evals-arm",
            "--history", str(evals_history()),
            "--events", str(state_dir() / "events.jsonl"),
            "--fno-bin", _resolve_fno_binary(),
            "--schedule-days", str(days),
            "--stale-days", str(int(getattr(evals_cfg, "stale_days", 7) or 7)),
            "--summary-json", json.dumps(evals_health_summary(evals_history())),
        ]
        proc = subprocess.run(argv, capture_output=True, text=True, check=False,
                              timeout=max(1.0, seconds_left_fn() or 30.0))
        if proc.returncode != 0:
            raise RuntimeError(f"evals-arm exited {proc.returncode}: {proc.stderr[:160]}")
        answer = json.loads(proc.stdout.strip().splitlines()[-1]) if proc.stdout.strip() else {}
        row(answer.get("skip_reason"), str(answer.get("detail") or ""), int(answer.get("acted") or 0))
    except Exception as exc:  # noqa: BLE001 - never let the arm break the tick
        row("arm_failed", f"{type(exc).__name__}: {exc}")


def _watchdog_recovery_roots() -> list[Path]:
    """Resolve every distinct project scope for the launchd watchdog scan.

    One shared resolver with the manual report verb
    (unfinished_work.report_roots): the tick and a hand-run must name the
    same fleet, or the two report surfaces disagree on scope."""
    from fno.agents import unfinished_work as _uw

    return _uw.report_roots()


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

        The read routes by key to the global root, so a live node claim is
        seen from any cwd; only an exception falls back to True (treat as
        live) to avoid double-dispatch onto a node a live /target session
        owns.
        """
        try:
            info = claim_status(f"node:{node_id}")
            # live OR suspect: a suspect claim (TTL-unexpired, dead pid)
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

#: A roster probe needs at least this much budget to be worth starting. The
#: probe measured 3.4s on a 43-row fleet, so anything under this buys a
#: certain timeout rather than a smaller answer.
_ROSTER_FLOOR_S = 8.0


class _WatchdogBudgetSpent(Exception):
    """The tick has too little left to sweep. Not a failure of the sweep."""


#: A wake apply needs this much tick left before it may start: `fno
#: agents resume` waits up to 180s and the landing confirmation polls
#: after it. Starting one with less is how the watchdog leg eats the
#: legs behind it.
_WAKE_APPLY_FLOOR_S = 200


def _wd_apply_and_emit(wd, verdict, *, cwd: str, agent: str, label: str) -> str:
    try:
        outcome, detail = wd.apply_verdict(verdict, lanes="wake", cwd=cwd, agent=agent)
    except Exception as exc:  # noqa: BLE001 - one row never aborts the rest
        outcome, detail = "refused", f"{label} crashed: {exc!r}"
    wd.emit_event(
        wd.outcome_event(outcome),
        {"row_id": verdict.row_id, "verdict": verdict.verdict, "detail": detail,
         "outcome": outcome},
    )
    return outcome

#: A stranded sweep is one batched git fetch plus a rev-list and a
#: last-commit-age call per worktree - cheap, but not free at 60+
#: worktrees. Skipping under this floor costs nothing: the next tick
#: sweeps from scratch, there is no partial state to lose.
_STRANDED_FLOOR_S = 10.0

#: 0.8s per root measured 2026-09-02 over 18 roots and 3113 rollouts (14.9s
#: for the leg); the floor carries headroom for a fatter-than-mean root.
#: Skipping under it costs nothing - the next tick starts the scan over.
_RECOVERY_ROOT_FLOOR_S = 3.0

#: Per-phase alarm caps : each phase runs under its own slice,
#: min(cap, seconds left before the tick ceiling). Every-tick caps are p90s
#: of 50 measured ticks (2026-09-17, events.jsonl), rounded up; sweep and
#: merge are the epic's core work and keep their measured room. The fleet
#: tail (stranded, recovery, watchdog) runs one phase per tick - the
#: _run_phase cadence - so the worst tick is the every-tick sum plus the
#: largest fleet cap. The fit is computed, never restated:
#: test_phase_caps_fit_ceiling fails the suite when an edit breaks it (the
#: hand-written sum this table replaced claimed 550s; the table itself
#: summed to 730 against the 480s ceiling).
_EVERY_TICK_CAP_S: dict[str, float] = {
    "settings": 10,
    "sweep": 150,
    "merge": 120,
    "king_wake": 45,
    "notify_watch": 10,
    "heal": 10,
    "evals": 10,
}
_FLEET_CAP_S: dict[str, float] = {
    "stranded": 60,
    "recovery": 90,
    "watchdog": 30,
}
_PHASE_CAP_S: dict[str, float] = {**_EVERY_TICK_CAP_S, **_FLEET_CAP_S}


class TickDeadlineExceeded(BaseException):
    """The tick's wall-clock deadline fired; the phase marker names where.

    BaseException on purpose: every broad `except Exception` seam in the tick
    path (the sweep, recovery) exists to degrade one leg without
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
    from fno.pr_watch._dispatch import (
        current_tick_phase,
        phase_seconds_left,
        set_phase_deadline,
        set_tick_phase,
    )
    from fno.pr_watch._dispatch import SCAN_PROGRESS as sweep_progress
    from fno.pr_watch._dispatch import tick as _tick
    from fno.pr_watch._install import tick_end_bits

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

    from fno.loops import loops_paused

    if loops_paused():
        _emit_event(
            "pr_watch_tick_end",
            {
                "outcome": "paused",
                "duration_s": round(time.monotonic() - started, 3),
                "phase": "entry",
                "pid": os.getpid(),
            },
        )
        return

    outcome = "error"
    result = None
    tick_failed = None
    timed_out = False
    settings = None
    cfg = None
    tick_enabled = False
    sweep_started = False
    alarm_ok = True
    cut: list[str] = []
    phase_s: dict[str, float] = {}
    # Per-phase partial-work notes a body writes as it goes; the runner
    # clears one per phase run and emits it on the cut path, so a cut phase
    # hands back what it did. The sweep's note lives across the _dispatch
    # boundary (SCAN_PROGRESS); this dict holds the rest.
    progress: dict[str, str] = {}
    ceiling_box: dict[str, Optional[int]] = {"v": None}
    arm_interval: dict[str, int] = {"king_wake": 900, "notify_watch": 300, "watchdog": 600}
    roots_box: dict[str, Optional[list]] = {"v": None}

    # One sidecar scan per tick: four phases each swept the same roots.
    def _tick_roots() -> list:
        roots = roots_box["v"]
        if roots is None:
            roots = _catchup_roots()
            roots_box["v"] = roots
        return roots

    try:
        try:
            signal.signal(signal.SIGALRM, _on_deadline)
        except ValueError:
            # Not the main thread (tests embedding the command): no alarm
            # available, run unbounded like before.
            alarm_ok = False
            log.debug("pr-watch: SIGALRM unavailable outside main thread")

        # A bootout kills this process by signal; without a handler the tick
        # dies with no record.
        def _on_sigterm(signum, frame) -> None:  # noqa: ARG001 - handler signature
            signal.signal(signum, signal.SIG_IGN)
            phase = current_tick_phase()
            sender = _bounce_sender()
            _emit_event("pr_watch_tick_end", {
                "outcome": "error", "why": "killed",
                "duration_s": round(time.monotonic() - started, 3),
                "phase": phase, "pid": os.getpid(), "sender": sender,
            })
            _emit_tick_row(
                "pr_watch_merge",
                interval_s=int(getattr(cfg, "interval_seconds", 600)) if cfg is not None else 600,
                skip_reason="error",
                detail=(f"killed by a signal mid-tick; started and did not complete, "
                        f"phase={phase}; sender={sender}"),
            )
            signal.signal(signum, signal.SIG_DFL)
            os.kill(os.getpid(), signum)

        try:
            signal.signal(signal.SIGTERM, _on_sigterm)
        except ValueError:
            pass

        # One alarm per phase: every body runs under its own slice, so
        # a slow phase loses its turn instead of aborting the phases after it.
        # The runner is the only place that catches TickDeadlineExceeded.
        def _run_phase(
            name: str,
            body: Callable[[float], None],
            *,
            arm: Optional[str] = None,
            cadence: int = 1,
            slot: int = 0,
        ) -> bool:
            # Fleet-tail cadence: a phase on a cadence above 1 runs one tick
            # in `cadence`, on its slot of the interval bucket. A skipped
            # phase is not a failure: it mints an off_cadence arm row naming
            # the tick it next runs on, and its phase_s reads 0.0 without a
            # cut entry, so the tick outcome never reads timeout for a phase
            # that never ran.
            if cadence > 1 and cfg is not None:
                interval = max(1, int(getattr(cfg, "interval_seconds", 600)))
                tick_no = int(time.time() // interval)
                if tick_no % cadence != slot:
                    phase_s[name] = 0.0
                    progress.pop(name, None)
                    sweep_progress.pop(name, None)
                    if arm is not None:
                        next_tick = tick_no + (slot - tick_no) % cadence
                        _emit_tick_row(arm, interval_s=arm_interval.get(arm, interval),
                                       skip_reason="off_cadence",
                                       detail=(f"fleet-tail cadence {cadence}: "
                                               f"next tick {next_tick}"))
                    return False
            # A stale note from a previous tick must never ride this run's
            # receipt: a cut before the body ran reports zero progress.
            progress.pop(name, None)
            sweep_progress.pop(name, None)
            left: Optional[float] = None
            if ceiling_box["v"] is not None:
                left = ceiling_box["v"] - (time.monotonic() - started)
            if left is not None and left <= 0:
                cut.append(name)
                phase_s[name] = 0.0
                if arm is not None:
                    _emit_tick_row(arm, interval_s=arm_interval.get(arm, 600),
                                   skip_reason="timeout",
                                   detail=f"deadline exceeded before phase {name}")
                return False
            if ceiling_box["v"] is None:
                # The settings phase runs before a ceiling exists: its slice
                # is its measured cap, min(the env seam) when the seam is set.
                slice_s = float(_PHASE_CAP_S.get("settings", 60.0))
                env = (os.environ.get(_ENV_TICK_TIMEOUT) or "").strip()
                if env.isdigit() and int(env) > 0:
                    slice_s = min(slice_s, float(env))
            else:
                assert left is not None
                slice_s = min(_PHASE_CAP_S.get(name, left), left)
            # Which budget fired if the alarm does: a cap below the
            # remaining wall starves one phase; the wall is the tick deadline.
            cap = _PHASE_CAP_S.get(name)
            wall_limited = (
                ceiling_box["v"] is None or cap is None or cap >= (left or 0.0)
            )
            slice_s = max(1.0, slice_s)
            phase_start = time.monotonic()
            try:
                if alarm_ok:
                    signal.alarm(max(1, int(slice_s)))
                set_tick_phase(name)
                set_phase_deadline(time.monotonic() + slice_s)
                body(slice_s)
            except TickDeadlineExceeded:
                cut.append(name)
                if arm is not None:
                    # Name the sub-step the alarm caught: a phase that reports
                    # its halves reads as one stall, not a black box. And hand
                    # back what the phase did before the cut - the body's
                    # progress note or the sweep's scan counter - so the row
                    # says "scanned 21 of 39", never a bare timeout.
                    step = current_tick_phase()
                    at = f" at {step}" if step.startswith(name + ":") else ""
                    base = (f"deadline exceeded in phase {name} at "
                            f"{int(slice_s)}s" if wall_limited else
                            f"phase slice {int(slice_s)}s spent") + at
                    note = progress.get(name) or sweep_progress.get(name) or ""
                    _emit_tick_row(arm, interval_s=arm_interval.get(arm, 600),
                                   skip_reason="timeout",
                                   detail=f"{base} {note}" if note else base)
            finally:
                if alarm_ok:
                    try:
                        signal.alarm(0)
                    except ValueError:
                        pass
                set_phase_deadline(None)
                phase_s[name] = round(time.monotonic() - phase_start, 1)
            return True

        def _phase_settings(_slice_s: float) -> None:
            nonlocal settings, cfg, tick_enabled
            settings = load_settings()
            cfg = settings.pr_watch
            # wave 3: the master panic switch outranks pr_watch's own gate too.
            tick_enabled = cfg.enabled and settings.autonomy.enabled
            ceiling_box["v"] = _resolve_tick_deadline(cfg)

        _run_phase("settings", _phase_settings)

        # Phase order: PR legs first (sweep, king_wake, notify_watch, heal,
        # stranded), then the fleet-health tail (recovery, watchdog) - per-phase slices removed the shared deadline that gave recovery a head-of-line pass.
        def _phase_recovery(_slice_s: float) -> None:
            assert settings is not None and cfg is not None
            set_tick_phase("recovery")
            _fleet_candidates = 0
            _fleet_refused = 0
            _fleet_silent = 0
            _fleet_swept = False
            if settings.recovery.enabled and settings.autonomy.enabled:
                try:
                    from fno.recovery import run_recovery_sweep

                    def emit_recovery(event_type: str, data: dict) -> None:
                        nonlocal _fleet_refused
                        if event_type == "worker_refused":
                            _fleet_refused += 1
                        _emit_event(event_type, data)

                    _fleet_candidates = run_recovery_sweep(
                        settings.recovery,
                        emit=emit_recovery,
                    )
                    _fleet_swept = True
                    typer.echo(f"recovery sweep: candidates={_fleet_candidates}")
                except Exception as exc:  # noqa: BLE001 - never let recovery break pr-watch
                    log.warning("pr-watch: recovery sweep failed: %s", exc)

                # The cadence-deadline backstop, for a refusal the taxonomy does
                # not recognise. It reads the FULL registry, which the recovery
                # sweep's candidate set does not: that set drops every non-claude
                # row, so a codex successor is invisible to it. Report only - this
                # leg stops, spawns and unclaims nothing. Wrapped separately from
                # the recovery sweep so neither takes the other down.
                try:
                    from fno.agents.sweep import run_sweep as _run_silence_sweep

                    _rows, _fleet_silent = _run_silence_sweep(emit=_emit_for_sweep)
                    if _fleet_silent:
                        typer.echo(f"silence sweep: silent={_fleet_silent}")
                except Exception as exc:  # noqa: BLE001 - a backstop never breaks the tick
                    log.warning("pr-watch: silence sweep failed: %s", exc)

                # The fleet leg's own watermark and its liveness proof. `fno do pr watch
                # status` reported the agent loaded through a six-hour outage, so a
                # status line is not evidence that anything ticked; a file with a
                # timestamp is.
                #
                # Written only when the sweep COMPLETED. A failed sweep that still
                # stamped a watermark would render as a healthy quiet fleet -
                # candidates=0, refused=0, fresh timestamp - which is the exact
                # absence-as-evidence shape this whole node exists to kill. The
                # missing write turns a broken sweep into loud staleness inside two
                # ticks instead.
                if _fleet_swept:
                    try:
                        from fno.fleet_state import write_heartbeat

                        write_heartbeat(
                            candidates=_fleet_candidates, refused=_fleet_refused,
                            silent=_fleet_silent,
                        )
                    except Exception as exc:  # noqa: BLE001 - never fatal to the PR legs
                        log.warning("pr-watch: fleet heartbeat write failed: %s", exc)

        def _phase_watchdog(_slice_s: float) -> None:
            assert settings is not None and cfg is not None
            set_tick_phase("watchdog")
            # Imported here, not at module scope: the watchdog package pulls the
            # harness layer and this module is on the launchd hot path.
            from fno.agents.watchdog import lane_armed as _wd_lane_armed
            from fno.agents.watchdog import lane_off_detail as _wd_lane_off_detail
            from fno.agents.watchdog import wake_armed as _wd_wake_armed

            # Fleet watchdog, same cadence, same non-fatal wrap. The REPORT is
            # the unfinished-work snapshot (the operator's outcome question);
            # the internal session classifier runs only in wake mode, where it
            # may resume a positively stalled session. No tick value reaps or
            # reroutes - those stop a session and stay behind a manual
            # `fno agents watchdog --apply-all`.
            # getattr with the modeled default: a settings stub or a partially-loaded
            # config must never crash the tick - "off" is the no-op that fails safe.
            wd_i = settings.pr_watch.interval_seconds
            arm_interval["watchdog"] = wd_i
            if _wd_lane_armed(settings):
                acted = 0
                try:
                    import time as _time

                    from fno.agents import unfinished_work as _uw
                    from fno.agents import watchdog as _wd

                    now = _time.time()
                    # The tick's deadline is fatal and the report honors it by
                    # leaving late roots unscanned: their dimensions read
                    # unknown, never clean, and no partial snapshot is stamped
                    # or mailed as complete.
                    left = phase_seconds_left() or 0.0
                    budget = left / 2
                    roots = _watchdog_recovery_roots()
                    if not roots:
                        from fno.paths import resolve_repo_root

                        roots = [Path(resolve_repo_root())]
                    snapshot = _uw.build_report(
                        roots,
                        now_s=now,
                        deadline_monotonic=time.monotonic() + max(0.0, budget),
                    )
                    mail_to = str(settings.recovery.watchdog.mail_to or "")
                    _uw.publish_report(
                        snapshot,
                        source="tick",
                        now_s=now,
                        mail_to=mail_to,
                        log=lambda line: log.warning("pr-watch: %s", line),
                    )

                    # Provider-outage supervision, both modes, measured ONCE per
                    # tick: a breaker must be visible from a plain report tick -
                    # waiting for someone to arm wake mode is how the fleet stays
                    # blind through an outage. The wake sweep below reuses this
                    # measurement through run_sweep's provider_outage_fn seam, so
                    # no transcript is read twice on one tick. Refused the same
                    # way the wake lane is: a budget under the probe's measured
                    # cost buys a guaranteed timeout, not a smaller answer.
                    left = phase_seconds_left() or 0.0
                    if left < _ROSTER_FLOOR_S:
                        raise _WatchdogBudgetSpent(
                            f"{left:.1f}s left, under the {_ROSTER_FLOOR_S:.0f}s "
                            f"a roster probe costs"
                        )
                    provider_rows, _provider_warnings = _wd.fleet_rows(timeout=left)
                    provider_outages = _wd.measure_provider_outages(
                        provider_rows, now_s=now
                    )
                    # Read BEFORE any write, defaulted here: the sweep file is the
                    # only memory of what the event lane already said, so a first
                    # tick after a wipe re-announces the open breaker - correct.
                    prev_events_sig = _wd._last_events_signature()
                    previous_parts = set(filter(None, prev_events_sig.split(";")))
                    emitted_breaker_parts = []
                    for breaker in provider_outages.get("breakers") or []:
                        breaker_part = (
                            "provider-breaker:"
                            f"{breaker.get('provider')}:{breaker.get('account')}:"
                            f"{breaker.get('outage_epoch')}"
                        )
                        if breaker_part not in previous_parts:
                            _wd.emit_event("provider_breaker_transition", {
                                "outage_epoch": str(breaker.get("outage_epoch") or ""),
                                "provider": str(breaker.get("provider") or ""),
                                "account": str(breaker.get("account") or ""),
                                "phase": "open",
                                "count": len(breaker.get("row_ids") or []),
                            })
                        emitted_breaker_parts.append(breaker_part)
                    _wd.write_sweep_file(
                        "tick", None, now, None,
                        events_signature=";".join(
                            sorted(previous_parts | set(emitted_breaker_parts))
                        ),
                        provider_outages=provider_outages,
                    )
                    # Provider-outage handoff supervision moved to the
                    # provider-cap actor; measure_provider_outages
                    # stays as report lines only.

                    # Internal recovery, wake mode only. Session verdicts drive
                    # nothing here in report mode, and their receipts stay
                    # separate from the report's event stream.
                    recoverable_results = []
                    if _wd_wake_armed(settings):
                        # Recompute the budget AFTER the report: budgeting both
                        # halves off the same pre-report clock lets the wake lane
                        # spend the whole remaining deadline and SIGALRM every
                        # later tick leg.
                        wake_budget = (phase_seconds_left() or 0.0) / 2
                        if wake_budget < _ROSTER_FLOOR_S:
                            # A budget under the probe's measured cost buys a
                            # guaranteed timeout, not a smaller answer. Skip the
                            # lane; the next tick re-classifies.
                            log.warning(
                                "pr-watch: watchdog wake budget spent (%.1fs left "
                                "for the next tick)", wake_budget,
                            )
                        else:
                            payload, rows = _wd.run_sweep(
                                now_s=now, roster_timeout=wake_budget,
                                provider_outage_fn=lambda: provider_outages,
                            )
                            if payload.get("refused"):
                                # zero rows read is an instrument failure.
                                # No events, no gates - the refusal reads loud.
                                log.warning(
                                    "pr-watch: watchdog sweep refused: %s (%s)",
                                    payload["refused"],
                                    "; ".join(payload.get("warnings") or [])
                                    or "no cause given",
                                )
                                payload = {"verdicts": [], "counts": {}, "warnings": []}
                                rows = []
                            prev_recovery_sig = _wd._last_recovery_events_signature()
                            fresh_recovery_ids = _wd.fresh_non_leave(
                                payload, prev_recovery_sig
                            )
                            for d, row in zip(payload["verdicts"], rows):
                                verdict = _wd.Verdict(**d)
                                if verdict.verdict == _wd.LEAVE:
                                    continue
                                # fresh_non_leave answers ROW IDS; the gate is on the
                                # row, not the verdict word.
                                if verdict.row_id in fresh_recovery_ids:
                                    _wd.emit_event(
                                        "watchdog_verdict",
                                        {
                                            "row_id": verdict.row_id,
                                            "name": verdict.name,
                                            "verdict": verdict.verdict,
                                            "basis": verdict.basis,
                                        },
                                    )
                                if verdict.verdict == _wd.WAKE:
                                    # Budgeting only the PROBE left the expensive half
                                    # unbounded: one resume waits up to 180s and the
                                    # confirmation polls after it, so a few stuck rows
                                    # walk past the tick deadline and SIGALRM kills every
                                    # leg behind this one. A row skipped here is not
                                    # lost - the next tick re-classifies it.
                                    if (phase_seconds_left() or 0.0) < _WAKE_APPLY_FLOOR_S:
                                        log.warning(
                                            "pr-watch: watchdog wake budget spent, "
                                            "%s left for the next tick", verdict.row_id,
                                        )
                                        continue
                                    _wd_apply_and_emit(_wd, verdict, cwd=row.cwd, agent=row.agent, label="wake")
                                    acted += 1
                            # SILENCE lane: registry-scoped rows fleet_rows misses.
                            if (phase_seconds_left() or 0.0) < _WAKE_APPLY_FLOOR_S:
                                log.warning("pr-watch: watchdog silence budget spent")
                            else:
                                try:
                                    silence_vs, silence_rows_out = _wd.silence_verdicts(roots, now_s=now)
                                except Exception as exc:  # noqa: BLE001 - a broken lane never aborts the tick
                                    log.warning("pr-watch: silence sweep failed: %s", exc)
                                    silence_vs, silence_rows_out = [], []
                                for silence_v, silence_row in zip(silence_vs, silence_rows_out):
                                    if silence_v.verdict != _wd.SILENCE:
                                        continue
                                    if (phase_seconds_left() or 0.0) < _WAKE_APPLY_FLOOR_S:
                                        log.warning("pr-watch: watchdog silence budget spent")
                                        break
                                    _wd_apply_and_emit(_wd, silence_v, cwd=silence_row.cwd,
                                                        agent=silence_row.agent, label="silence drive")
                                    acted += 1
                            recovery_scans = []
                            recovery_roots_done = 0
                            for recovery_root in roots:
                                # Per root, not once before the loop: the stranded
                                # sweep learned this from a review finding and this
                                # leg never got the same check.
                                recovery_left = phase_seconds_left() or 0.0
                                if recovery_left < _RECOVERY_ROOT_FLOOR_S:
                                    log.info(
                                        "pr-watch: Codex recovery scan stopped after %d "
                                        "root(s), %.1fs left, under the %.0fs a scan "
                                        "costs - remaining roots retry next tick",
                                        recovery_roots_done,
                                        recovery_left,
                                        _RECOVERY_ROOT_FLOOR_S,
                                    )
                                    break
                                try:
                                    (
                                        recovery_payload,
                                        _recovery_rows,
                                        recovery_scan,
                                    ) = _wd.run_recoverable_sweep(
                                        cwd=recovery_root,
                                        recency_seconds=24 * 3600,
                                        now_s=now,
                                    )
                                except Exception as exc:  # noqa: BLE001 - never fatal
                                    log.warning(
                                        "pr-watch: Codex recovery scan failed: %s", exc
                                    )
                                    continue
                                if not recovery_scan.complete:
                                    log.warning(
                                        "pr-watch: Codex recovery scan refused for %s",
                                        recovery_root,
                                    )
                                    continue
                                recovery_scans.append((recovery_root, recovery_scan))
                                # After the work, like the stranded sweep's own
                                # counter: a root that raised or refused was
                                # attempted, never done.
                                recovery_roots_done += 1
                            for recovery_root, recovery_scan in recovery_scans:
                                results = _wd.apply_recoverable(
                                    recovery_scan,
                                    scope_cwd=recovery_root,
                                    should_apply=lambda: (
                                        phase_seconds_left() or 0.0
                                    ) >= _WAKE_APPLY_FLOOR_S,
                                )
                                recoverable_results.extend(results)
                                for result_item in results:
                                    # A non-applied recovery candidate is refound and
                                    # re-decided by every tick until it ages out of
                                    # the recency window; publish it once per recovery
                                    # signature, not once per 600s forever. An applied
                                    # row registered and never recurs. A deferred row
                                    # was never attempted, so there is nothing to say.
                                    if result_item["outcome"] == "applied" or (
                                        result_item["outcome"] != "deferred"
                                        and result_item["session_id"] in fresh_recovery_ids
                                    ):
                                        _wd.emit_event(
                                            _wd.outcome_event(result_item["outcome"]),
                                            {
                                                "row_id": result_item["session_id"],
                                                "verdict": _wd.RECOVERABLE,
                                                "detail": result_item["detail"],
                                                "outcome": result_item["outcome"],
                                            },
                                        )
                            # Stamp the recovery receipt gate: what was published
                            # plus what was already published, minus deferred sids a
                            # published-before-apply never actually said.
                            deferred_sids = {
                                item["session_id"]
                                for item in recoverable_results
                                if item["outcome"] == "deferred"
                            }
                            published_sids = {
                                item["session_id"]
                                for item in recoverable_results
                                if item["outcome"] != "deferred"
                            }
                            parts = [
                                part
                                for part in _wd.union_signature(
                                    prev_recovery_sig,
                                    _wd.verdict_signature(
                                        {
                                            **payload,
                                            "verdicts": [
                                                d for d in payload["verdicts"]
                                                if _wd.Verdict(**d).verdict != _wd.LEAVE
                                            ],
                                        }
                                    ),
                                ).split(";")
                                if part
                            ]
                            parts.extend(
                                f"{sid}:{_wd.RECOVERABLE}"
                                for sid in sorted(published_sids)
                            )
                            recovery_sig = ";".join(
                                part
                                for part in parts
                                if ":" not in part
                                or part.split(":", 1)[0] not in deferred_sids
                            )
                            _wd.write_sweep_file(
                                "tick",
                                None,
                                now,
                                None,
                                recovery_events_signature=recovery_sig,
                                provider_outages=provider_outages,
                            )
                    counts = " ".join(
                        f"{k}={v}"
                        for k, v in _uw.snapshot_payload(snapshot)["counts"].items()
                        if v is not None
                    )
                    recoverable_applied = sum(
                        item["outcome"] == "applied" for item in recoverable_results
                    )
                    recoverable_remaining = sum(
                        item["outcome"] == "deferred" for item in recoverable_results
                    )
                    typer.echo(
                        f"watchdog report: {counts} acted={acted} "
                        f"recoverable_applied={recoverable_applied} "
                        f"recoverable_remaining={recoverable_remaining}"
                    )
                    _emit_tick_row("watchdog", interval_s=wd_i, acted=acted,
                                   detail=f"{counts} recoverable_applied={recoverable_applied} "
                                          f"recoverable_remaining={recoverable_remaining}")
                except _WatchdogBudgetSpent as exc:
                    log.info("pr-watch: watchdog leg skipped: %s", exc)
                    _emit_tick_row("watchdog", interval_s=wd_i, skip_reason="budget_spent",
                                   detail=str(exc)[:200])
                except Exception as exc:  # noqa: BLE001 - never let the watchdog break pr-watch
                    log.warning("pr-watch: watchdog sweep failed: %s", exc)
                    _emit_tick_row("watchdog", interval_s=wd_i, skip_reason="sweep_failed",
                                   detail=str(exc)[:200])
            else:
                # An unarmed lane still ticks: "why it did nothing" is the readout's job.
                # The detail names the key that read false, not just the lane.
                _emit_tick_row("watchdog", interval_s=wd_i, skip_reason="watchdog_off",
                               detail=_wd_lane_off_detail(settings))

        def _phase_sweep(slice_s: float) -> None:
            nonlocal result, tick_failed
            assert settings is not None and cfg is not None
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
                    dispatch_deadline=time.monotonic() + slice_s,
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
                    # The skip can follow a sweep with failed repos, and this stdout
                    # line is what an operator tails during an outage: the failure
                    # count rides the skip line too, matching the end record.
                    degraded = (
                        f" (degraded: {result.sweep_failures} sweep failure(s))"
                        if result.sweep_failures
                        else ""
                    )
                    typer.echo(
                        f"pr-watch tick: graphql remaining {result.quota_remaining} below floor"
                        f" - dispatch pass skipped{reset}{degraded}"
                    )
                elif result.sweep_failures:
                    typer.echo(
                        f"pr-watch tick: degraded: {result.sweep_failures} sweep failure(s)"
                    )
                else:
                    typer.echo(
                        f"pr-watch tick: open_prs={result.open_prs} acted={result.acted} skipped={result.skipped}"
                    )

        def _phase_merge(slice_s: float) -> None:
            assert cfg is not None
            set_tick_phase("merge")
            interval = int(getattr(cfg, "interval_seconds", 600))
            head = f"merge sweep={'cut' if 'sweep' in cut else 'ok'}"
            if not tick_enabled:
                _emit_tick_row("pr_watch_merge", interval_s=interval, skip_reason="disabled",
                               detail=f"{head} pr_watch disabled")
                return
            from fno.pr_watch._discover import PrCandidate
            from fno.pr_watch._dispatch import run_execute_queue
            from fno.pr_watch._state import make_watermark_key
            from fno.rust_binary import VerbUnavailable, verb_call

            roots = _tick_roots()
            try:
                # Durable grants, never the sweep's result: a cut sweep leaves
                # no result, and a completed one reads few PRs under load.
                # Slice-derived, minus the same 10s reserve _ritual_timeout
                # keeps: the read expires as a recorded failure BEFORE the
                # phase alarm. The old 60s literal spent a third of the slice
                # learning only that the store was contended.
                out = verb_call("authorized-merge", {"op": "grant-queue",
                                "rotate": int(time.time() // interval),
                                "cwd": str(roots[0] if roots else Path.cwd())},
                                timeout=max(1.0, slice_s - 10.0))
                if out.get("error"):
                    raise VerbUnavailable(str(out["error"]))
                queue = [
                    (PrCandidate(node_id=r["node_id"], pr_number=int(r["pr"]), pr_url=None,
                                 repo_dir=Path(r["cwd"]), repo_slug=r["repo_slug"]),
                     make_watermark_key(repo_slug=r["repo_slug"], pr_number=int(r["pr"])),
                     r.get("grant") or {})
                    for r in out.get("queue") or []
                ]
                progress["merge"] = f"queue={len(queue)}"
            except (VerbUnavailable, KeyError, TypeError, ValueError) as exc:
                _emit_tick_row("pr_watch_merge", interval_s=interval, skip_reason="error",
                               detail=f"{head} grant queue unreadable ({exc})")
                return
            counts = run_execute_queue(
                queue, emit=_emit_event,
                notify=lambda message, **_kw: _notify_parked(message),
                max_retries=cfg.retries, claim=ClaimAdapter(),
            )
            verdicts = out.get("verdicts") or {}
            detail = (f"{head} candidates={out.get('candidates', 0)} granted={verdicts.get('granted', 0)} "
                      + " ".join(f"{k}={v}" for k, v in counts.items())
                      + f" read_ms={out.get('elapsed_ms', -1)}")
            _emit_tick_row("pr_watch_merge", interval_s=interval,
                           acted=counts["executed"], detail=detail)


        # Stranded-worktree recovery, same arming gate as the fleet
        # watchdog above: this is a second read of the same "is recovery
        # armed" decision, not a second dispatcher - config.recovery.watchdog
        # + recovery.enabled + autonomy.enabled all still gate whether
        # anything here acts. Report, never reap: only STRANDED rows get
        # pushed and filed; only UNKNOWN rows get recorded; every other
        # class, LIVE included, is quiet and untouched.
        #
        # The king wake phase runs BEFORE it: it is cheaper than either leg
        # (one registry read, one transcript probe per crown, a bus scan) and
        # the wake it fires is the thing the stranded sweep would otherwise
        # have to notice too late.
        def _phase_king_wake(_slice_s: float) -> None:
            set_tick_phase("king_wake")
            # The guard is the first statement, before the import: this module is
            # on the launchd hot path and the wake phase pulls the bus and the
            # harness layer, which an unarmed tick must not pay for. The double
            # getattr matches the phase's own read: a settings stub with no king
            # block at all (the tick's test harnesses) must read as unarmed.
            # Double getattr throughout: a settings stub with no king block at all
            # must read as unarmed (debounce default), never crash the tick.
            kw_i = int(getattr(getattr(settings, "king", None), "wake_debounce_seconds", 900))
            arm_interval["king_wake"] = kw_i
            if getattr(getattr(settings, "king", None), "wake_enabled", False):
                try:
                    from fno.pr_watch._king_wake import run_king_wake

                    wake_summary = run_king_wake(
                        settings,
                        emit=_emit_event,
                        seconds_left_fn=phase_seconds_left,
                        on_step=lambda s: set_tick_phase(f"king_wake:{s}"),
                    )
                    woke = ", ".join(
                        f"{w['scope']}:{w['reason']}" for w in wake_summary.get("woke", [])
                    )
                    typer.echo(
                        f"king wake: crowns={wake_summary.get('crowns', 0)}"
                        + (f" woke={woke}" if woke else "")
                    )
                    crowns = int(wake_summary.get("crowns", 0) or 0)
                    woke_n = len(wake_summary.get("woke", []) or [])
                    evaluated = int(wake_summary.get("evaluated", 0) or 0)
                    truth_reads = int(wake_summary.get("truth_reads", 0) or 0)
                    if crowns == 0:
                        skip = "no_crowned_target"
                    elif woke_n:
                        skip = None
                    elif wake_summary.get("budget_spent"):
                        # The watchdog's own token: a pass that ran out of
                        # slice before it could act is not a failure.
                        skip = "budget_spent"
                    else:
                        skip = "no_trigger"
                    note = wake_summary.get("note")
                    detail = (
                        f"crowns={crowns} evaluated={evaluated}/{crowns}"
                        f" truth_reads={truth_reads}"
                        + (f" woke={woke}" if woke else "")
                        + (f" note={note}" if note else "")
                    )
                    _emit_tick_row("king_wake", interval_s=kw_i, acted=woke_n,
                                   skip_reason=skip, detail=detail)
                except Exception as exc:  # noqa: BLE001 - never let a wake break the tick
                    log.warning("pr-watch: king wake phase failed: %s", exc)
                    _emit_tick_row("king_wake", interval_s=kw_i, skip_reason="wake_failed",
                                   detail=str(exc)[:200])
            else:
                _emit_tick_row("king_wake", interval_s=kw_i, skip_reason="wake_disabled")

        # The operator-notice sampler: one phase, always run; the
        # Rust arm answers notify_off itself when the [notify] signals list
        # is empty, so the readout shows the arm whether or not it is armed.
        def _phase_notify(slice_s: float) -> None:
            _run_notify_watch_phase(_tick_roots(), timeout_s=max(1.0, slice_s - 2.0))

        # The heal drive loop: nothing called the healer on a timer, so every
        # red open PR waited for a hand. The loop lives in Rust; this phase is
        # only the gate. The arm guard lives inside run_heal_phase, and every
        # gate answer lands in the journal as a control_plane_tick row so the
        # status line can say why nothing ran.
        def _phase_heal(_slice_s: float) -> None:
            set_tick_phase("heal")
            try:
                from fno.pr_watch._heal_phase import run_heal_phase

                answer = run_heal_phase(settings, _tick_roots())
            except Exception as exc:  # noqa:BLE001 - never let heal break the tick
                log.warning("pr-watch: heal phase failed: %s", exc)
                return
            typer.echo(f"pr heal: {answer}")
            if answer != "ran":
                # The same arm row the detached spawn writes; this covers the
                # gate answers that never reach the binary.
                _emit_tick_row(
                    "heal",
                    interval_s=int(getattr(cfg, "interval_seconds", 600)),
                    acted=0,
                    skip_reason=answer.replace("-", "_"),
                    detail=f"auto_heal gate: {answer}",
                )

        def _phase_evals(_slice_s: float) -> None:
            set_tick_phase("evals")
            _run_evals_arm_phase(settings, seconds_left_fn=phase_seconds_left)

        def _phase_stranded(slice_s: float) -> None:
            assert settings is not None and cfg is not None
            # The watchdog def imports these for its own lanes; the stranded
            # sweep reads the same arming decisions, so it imports its own.
            from fno.agents.watchdog import lane_armed as _wd_lane_armed
            from fno.agents.watchdog import wake_armed as _wd_wake_armed
            set_tick_phase("stranded")
            # The sweep feeds the board's provenance cache; the lane only arms acting.
            lane_armed = _wd_lane_armed(settings)
            try:
                left = phase_seconds_left() or 0.0
                if left < _STRANDED_FLOOR_S:
                    raise _WatchdogBudgetSpent(
                        f"{left:.1f}s left, under the {_STRANDED_FLOOR_S:.0f}s "
                        "a stranded sweep costs"
                    )
                from fno.branch_provenance_cache import write_cache
                from fno.worktree_stranded import STRANDED, UNKNOWN, apply_sweep, sweep

                wake = lane_armed and _wd_wake_armed(settings)
                changed, stranded_n, unknown_n, acted_n, failed_n, roots_done = False, 0, 0, 0, 0, 0
                # Rotate the starting root by run: one stranded run every
                # three interval buckets (the fleet-tail cadence), so
                # consecutive runs - not buckets - start at consecutive
                # roots and a cap cut never replays the same prefix.
                roots = _tick_roots()
                if roots:
                    k = (int(time.time() // max(1, int(cfg.interval_seconds)))
                         // 3) % len(roots)
                    roots = roots[k:] + roots[:k]
                for root in roots:
                    # Re-check per root, not just once before the loop: a
                    # code-review finding caught that the floor above only
                    # bounded the FIRST root - a multi-repo tick with several
                    # catch-up roots could blow well past the shared tick
                    # deadline after the first root's own check passed.
                    left = phase_seconds_left() or 0.0
                    if left < _STRANDED_FLOOR_S:
                        log.info(
                            "pr-watch: stranded leg stopped after %d root(s), "
                            "%.1fs left, under the %.0fs a sweep costs - "
                            "remaining roots retry next tick",
                            roots_done, left, _STRANDED_FLOOR_S,
                        )
                        break
                    try:
                        stranded_rows = sweep(repo=root)
                        changed |= write_cache(root, stranded_rows)
                        outcomes = apply_sweep(stranded_rows, wake=wake)
                    except Exception as exc:  # noqa: BLE001 - one bad repo never stops the rest
                        log.warning("pr-watch: stranded sweep failed for %s: %s", root, exc)
                        continue
                    stranded_n += sum(1 for r in stranded_rows if r.klass == STRANDED)
                    unknown_n += sum(1 for r in stranded_rows if r.klass == UNKNOWN)
                    acted_n += len(outcomes)
                    failed_n += sum(1 for o in outcomes if o["stopped_at"])
                    roots_done += 1
                typer.echo(
                    f"stranded sweep ({'wake' if wake else 'report'}): "
                    f"stranded={stranded_n} unknown={unknown_n} "
                    f"acted={acted_n} failed={failed_n}"
                )
                if changed:
                    from fno.graph.render import render_graph_md
                    from fno.graph.store import read_graph_strict
                    render_graph_md(read_graph_strict())
            except _WatchdogBudgetSpent as exc:
                log.info("pr-watch: stranded leg skipped: %s", exc)
            except Exception as exc:  # noqa: BLE001 - never let the stranded sweep break pr-watch
                log.warning("pr-watch: stranded sweep failed: %s", exc)

        # Canonical-sync catch-up does NOT run on the tick: it duplicated
        # `fno backlog reconcile`'s SessionStart leg, and its sync shell is
        # where ticks died. Reconcile owns the outcome-keyed leg and surfaces
        # a proven-stale canonical through its SessionStart hook.
        sweep_started = True
        _run_phase("sweep", _phase_sweep, arm="pr_watch_sweep")
        _run_phase("merge", _phase_merge, arm="pr_watch_merge")
        _run_phase("king_wake", _phase_king_wake, arm="king_wake")
        _run_phase("notify_watch", _phase_notify, arm="notify_watch")
        _run_phase("heal", _phase_heal)
        _run_phase("evals", _phase_evals)
        # The fleet tail: each runs one tick in three, on its slot of the
        # interval bucket (the same rotation _phase_stranded uses for its
        # root start), so the PR lane never pays all three p90s in one tick.
        _run_phase("stranded", _phase_stranded, arm="stranded", cadence=3, slot=0)
        _run_phase("recovery", _phase_recovery, arm="recovery", cadence=3, slot=1)
        _run_phase("watchdog", _phase_watchdog, arm="watchdog", cadence=3, slot=2)
    except TickDeadlineExceeded:
        # Backstop: the per-phase runner catches its own cuts. Reaching here
        # means a cut escaped between phases; phase names where. This is the
        # one remaining timeout: the wall ceiling aborting the tick.
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
        # A cut phase no longer aborts the tick and no longer reads timeout:
        # its own row names the slice it spent, and a tick that ran to this
        # finally is not a timeout whatever a cap spent. Only the backstop
        # above - the wall ceiling firing between phases - is, and it still
        # exits 75 so launchd logs it without suppressing the successor.
        outcome = _tick_outcome(result, tick_failed, timed_out)
        end_data: dict[str, Any] = {
            "outcome": outcome,
            "duration_s": round(time.monotonic() - started, 3),
            "phase": cut[0] if cut else current_tick_phase(),
            "pid": os.getpid(),
        }
        # timed_out is the wall backstop now; slice overruns stay on the
        # cut/saturated fields and never reach why.
        if timed_out:
            end_data["why"] = "deadline_exceeded"
        if cut:
            end_data["cut"] = list(cut)
        if phase_s:
            end_data["phase_s"] = dict(phase_s)
        # Saturated = the alarm fired while the body ran, so the phase spent
        # its whole slice. A phase cut before its body ran spent zero: cut
        # names it, but it was starved, not saturated.
        end_data["saturated"] = [name for name in cut if phase_s.get(name, 0.0) > 0.0]
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
        # Arms-readout row for the dispatch legs: the sweep phase writes it at
        # its own end now, so this finally covers only the ticks where the
        # sweep phase never started (a settings error, say) and an error tick
        # stays visible.
        if not sweep_started:
            cfg_interval = int(getattr(cfg, "interval_seconds", 600)) if cfg is not None else 600
            bits = tick_end_bits(end_data)
            _emit_tick_row("pr_watch_merge", interval_s=cfg_interval,
                           acted=int(getattr(result, "acted", 0) or 0),
                           skip_reason=outcome if outcome in
                           ("disabled", "lock_held", "quota_skip", "error", "timeout") else None,
                           detail=f"outcome={outcome}" + (f" ({', '.join(bits)})" if bits else ""))

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
    enabled means running.  Pass ``--no-activate`` to write only.
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
    # A fresh install sees the healer's arm state beside the watcher's.
    from fno.pr_watch._install import heal_status_line

    typer.echo(heal_status_line())


@cli.command()
def refresh() -> None:
    """Re-render the plist onto the current binary and bounce the watcher.

    Non-interactive, no confirm prompt: this is the tail of ``fno doctor update`` (so
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
        defer_when_ticking=True,
        caller="refresh",
    )
    typer.echo(f"pr-watch refresh: {msg}")
    from fno.pr_watch._install import heal_status_line

    typer.echo(heal_status_line())


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
    from fno.claims.io import global_claims_root
    from fno.pr_watch import _install as m

    settings = load_settings()
    if not settings.pr_watch.enabled:
        typer.echo("pr-watch heal: disabled; nothing to heal")
        return

    # A bounce that has not had its first tick yet is not a wedge to cure:
    # bouncing again re-arms the healthy-pending grace over the same fault
    # and hides it for another 2x interval.
    try:
        report = m.liveness_report_live()
    except Exception:  # noqa: BLE001 - a probe that cannot read never blocks a cure
        report = {}
    if report.get("bounce_pending") is True:
        typer.echo(
            f"pr-watch heal: a bounce is pending its first tick ({report.get('detail')}); "
            "skipped. Run fno do pr watch refresh to bounce anyway."
        )
        return

    from fno.backlog.single_flight import acquire_flight

    heal_root = global_claims_root()
    flight = acquire_flight(
        "pr-watch:heal", scope="pr-watch SessionStart self-heal",
        name="pr-watch-heal", root=heal_root, ttl_ms=_HEAL_TTL_MS,
    )
    if flight is None or flight.held:
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
            defer_when_ticking=True,
            caller="heal",
        )
        typer.echo(f"pr-watch heal: {msg}")
        if rc != 0:
            raise typer.Exit(1)
    finally:
        flight.release()


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
