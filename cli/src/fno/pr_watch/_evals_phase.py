"""The pr-watch tick's evals phase (x-ab72): demand for the eval bank.

The bank is fully built and was still allowed to go quiet: 24 history rows,
then 40 days of silence, with both wired consumers displaying the stale 100%
as healthy. This phase is the forcing function. On the tick cadence, when
the newest regression-tier run is older than ``evals.schedule_days`` and the
spawn gate admits, it runs the regression tier and journals the outcome. It
gates nothing: the red row in ``fno doctor`` and ``fno backlog triage
health`` is the escalation, this leg is what clears it.

Graded outcomes are data (a failed task still lands in
``evals_scheduled_run``); plumbing failures (gate refusal, timeout, non-zero
exit, an exit-0 run that appended no rows) are ``evals_stale`` when the bank
has aged past twice ``evals.stale_days``, with an operator notice at most
once per schedule window. The events journal is the rate bound, so no new
state file exists to outlive its writer.
"""

from __future__ import annotations

import json
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

#: Refuse to start the run under this much remaining tick budget: the
#: regression tier's task timeouts total 25 minutes, so a smaller window buys
#: a killed subprocess and an orphaned worktree, not a fresh history row.
_MIN_RUN_BUDGET_S = 300

EVALS_SCHEDULED_RUN = "evals_scheduled_run"
EVALS_STALE = "evals_stale"


def _default_notify(title: str, message: str) -> None:
    """One OS notice through the same path ``fno inbox notify`` drives."""
    try:
        from fno.notify._impl import send_notification

        send_notification(title, message, "")
    except Exception:  # noqa: BLE001 - a notice lane never breaks the tick
        pass


def _gate_open() -> tuple[bool, str]:
    """Read-only spawn-gate admission: the same counters a spawn would be
    refused on, without consuming a slot (an eval run spawns no worker).

    A gate that cannot be read admits: the run is a local subprocess, not a
    worker, and a transient census failure is not evidence of pressure.
    """
    try:
        from fno.agents import spawn_gate
        from fno.config import load_settings

        agents_cfg = load_settings().agents
        if int(agents_cfg.max_live) - spawn_gate.census().slot_count <= 0:
            return False, "fleet_full"
        snapshot = spawn_gate._load_snapshot(float(agents_cfg.max_load_per_cpu))
        if snapshot.spawn_load_status == "exceeded":
            return False, "load"
    except Exception:  # noqa: BLE001 - an unreadable gate is not pressure
        return True, ""
    return True, ""


def _last_stale_journal_ts(events_path: Optional[Path]) -> Optional[datetime]:
    """Newest prior ``evals_stale`` ts in the global journal, or None.

    The journal IS the notice dedup (the king wake's ledger pattern): one
    operator mail per schedule window needs no state file of its own. Rows
    rotated away only ever over-notify once per rotation.
    """
    if events_path is None or not events_path.exists():
        return None
    try:
        lines = events_path.read_text(encoding="utf-8").splitlines()[-500:]
    except OSError:
        return None
    newest: Optional[datetime] = None
    for line in reversed(lines):
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if not isinstance(row, dict) or row.get("type") != EVALS_STALE:
            continue
        ts = row.get("ts")
        if not isinstance(ts, str):
            continue
        try:
            dt = datetime.fromisoformat(ts.strip().replace("Z", "+00:00"))
        except ValueError:
            continue
        if newest is None or dt > newest:
            newest = dt
    return newest


def _rows_since(
    history_path: Path, cutoff: datetime
) -> list[dict[str, object]]:
    """Regression rows appended after *cutoff*.

    Attribution is by each row's own timestamp, not a count delta: a
    concurrent evals writer's rows carry their own ts and stay out of this
    run's receipt. A failed read is an empty list, never a crash.
    """
    try:
        from fno.evals.report import _parse_ts, load_rows

        return [
            r for r in load_rows(history_path)
            if r.get("tier") == "regression"
            and (dt := _parse_ts(r.get("ts"))) is not None and dt >= cutoff
        ]
    except Exception:  # noqa: BLE001
        return []


def _run_subprocess(
    runner: Optional[Callable[..., subprocess.CompletedProcess]],
    cmd: list[str],
    timeout_s: int,
) -> subprocess.CompletedProcess:
    """The subprocess step, injectable so tests never execute the bank."""
    if runner is not None:
        return runner(cmd, timeout_s)
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout_s)


def run_evals_phase(
    settings: Any,
    *,
    emit: Callable[[str, dict[str, Any]], Any],
    budget_left_s: float,
    fno_bin: str = "fno-py",
    history_path: Optional[Path] = None,
    events_path: Optional[Path] = None,
    notify: Optional[Callable[[str, str], None]] = None,
    runner: Optional[Callable[..., subprocess.CompletedProcess]] = None,
    now: Optional[datetime] = None,
) -> dict[str, Any]:
    """Run the tick's evals leg; return its tick row fields.

    Never raises. The return dict carries ``acted`` (0/1), ``skip_reason``
    (None when it ran) and ``detail``. ``settings`` is read once for the
    evals block; the autonomy master switch gates here like every scheduled
    leg, and a settings stub with no evals block reads as unarmed.
    """
    if now is None:
        now = datetime.now(timezone.utc)
    if notify is None:
        notify = _default_notify
    if history_path is None:
        from fno.paths import evals_history

        history_path = evals_history()
    if events_path is None:
        try:
            from fno.paths import state_dir

            events_path = state_dir() / "events.jsonl"
        except Exception:  # noqa: BLE001 - no state dir, no dedup read
            events_path = None

    evals_cfg = getattr(settings, "evals", None)
    if evals_cfg is None:
        return {"acted": 0, "skip_reason": "evals_off",
                "detail": "no evals settings block"}
    schedule_days = int(getattr(evals_cfg, "schedule_days", 0) or 0)
    if schedule_days <= 0:
        return {"acted": 0, "skip_reason": "evals_off", "detail": "schedule_days 0"}
    stale_days = int(getattr(evals_cfg, "stale_days", 7) or 7)
    try:
        from fno.config import autonomy_master_enabled

        if not autonomy_master_enabled():
            return {"acted": 0, "skip_reason": "autonomy_off",
                    "detail": "config.autonomy.enabled is false"}
    except Exception:  # noqa: BLE001 - an unreadable master switch reads off
        return {"acted": 0, "skip_reason": "autonomy_off",
                "detail": "autonomy master unreadable"}

    # The demand read: is the newest regression run older than the window?
    # No history and no regression row are both DUE (the bank has never been
    # demanded); an unreadable age neither asserts staleness nor spends budget.
    from fno.evals.report import evals_health_summary

    try:
        summary = evals_health_summary(history_path, now=now)
    except Exception:  # noqa: BLE001 - an unreadable history reads unknown
        summary = None
    never_ran = summary is None or bool(summary.get("never_ran"))
    age_days: Optional[float] = None
    if summary is not None and not never_ran and summary.get("age_days") is not None:
        age_days = float(summary["age_days"])
    if not never_ran and age_days is None:
        return {"acted": 0, "skip_reason": "age_unknown",
                "detail": "history carries no readable regression timestamp"}
    if not never_ran and age_days is not None and age_days <= schedule_days:
        return {"acted": 0, "skip_reason": "fresh",
                "detail": f"age {age_days:.0f}d <= {schedule_days}d window"}

    def _escalates() -> bool:
        if never_ran:
            return True
        return age_days is not None and age_days > 2 * stale_days

    def _journal_stale(reason: str, detail: str) -> None:
        # Read the rate bound BEFORE emitting: this very row would otherwise
        # read back as the newest prior notice and swallow the mail forever.
        last = _last_stale_journal_ts(events_path)
        emitted = False
        try:
            emitted = bool(emit(EVALS_STALE, {
                "reason": reason,
                "detail": detail[:200],
                "age_days": age_days,
                "never_ran": never_ran,
                "window_days": schedule_days,
            }))
        except Exception:  # noqa: BLE001 - a journal failure never breaks the tick
            pass
        if last is not None and (now - last).total_seconds() < schedule_days * 86400:
            return
        if not emitted:
            # The journal IS the rate bound: a notice without its receipt row
            # is unverifiable state. The next tick re-journals and notifies.
            return
        notify(
            "fno evals stale",
            f"regression tier could not run ({reason}): {detail[:160]}. "
            "Run: fno doctor evals run --tier regression -y",
        )

    gate_ok, gate_reason = _gate_open()
    if not gate_ok:
        if _escalates():
            _journal_stale("gate", f"spawn gate refused: {gate_reason}")
        return {"acted": 0, "skip_reason": gate_reason,
                "detail": f"spawn gate refused: {gate_reason}"}

    timeout_s = int(budget_left_s - 60)
    if timeout_s < _MIN_RUN_BUDGET_S:
        return {"acted": 0, "skip_reason": "budget",
                "detail": f"{int(budget_left_s)}s of tick budget left, "
                          f"under the {_MIN_RUN_BUDGET_S}s a regression run costs"}

    # The run window opens at the phase's own clock so tests can pin it; the
    # child's rows are written after this instant in real time.
    started_wall = now
    started = time.monotonic()
    cmd = [fno_bin, "doctor", "evals", "run", "--tier", "regression", "-y"]
    try:
        proc = _run_subprocess(runner, cmd, timeout_s)
    except subprocess.TimeoutExpired:
        if _escalates():
            _journal_stale("timeout", f"killed at the {timeout_s}s tick budget")
        return {"acted": 0, "skip_reason": "run_timeout",
                "detail": f"timeout at {timeout_s}s"}
    except Exception as exc:  # noqa: BLE001 - spawn trouble is a stale cause
        if _escalates():
            _journal_stale("error", f"could not spawn {fno_bin}: {exc}")
        return {"acted": 0, "skip_reason": "run_failed",
                "detail": f"could not spawn {fno_bin}: {exc}"}
    duration_s = round(time.monotonic() - started, 3)
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "").strip().splitlines()[-1:] or [""]
        if _escalates():
            _journal_stale("error", f"exit {proc.returncode}: {tail[0]}")
        return {"acted": 0, "skip_reason": "run_failed",
                "detail": f"exit {proc.returncode}: {tail[0]}"}

    added_rows = _rows_since(history_path, started_wall)
    if not added_rows:
        # Exit 0 with no appended rows is the trap: a clean-looking run that
        # inspected nothing. A could-not-fire, never a success.
        if _escalates():
            _journal_stale("no_rows", "run exited 0 but appended no history rows")
        return {"acted": 0, "skip_reason": "no_rows",
                "detail": "exit 0, no rows appended"}
    passes = sum(1 for r in added_rows if r.get("pass") is True)
    task_count = len({r.get("task_id") for r in added_rows})
    try:
        emit(EVALS_SCHEDULED_RUN, {
            "task_count": task_count,
            "passes": passes,
            "duration_s": duration_s,
            "age_days_before": age_days,
            "window_days": schedule_days,
        })
    except Exception:  # noqa: BLE001 - a journal failure never breaks the tick
        pass
    return {"acted": 1, "skip_reason": None,
            "detail": f"{task_count} task(s), {passes}/{len(added_rows)} pass, "
                      f"{duration_s:.0f}s"}
