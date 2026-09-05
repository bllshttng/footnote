"""Python emitter for ``control_plane_tick`` rows.

The row shape is owned by ``crates/fno-agents/src/tick_ledger.rs`` and pinned
by ``cli/src/fno/events/schema.yaml``; this helper is the one Python-side
writer, so every Python arm emits the same fields through the same validated
path. Rows land in the global journal by default (the journal the scheduled
arms already write), and the arms readout in ``fno agents status`` folds them
into one row per arm.

Set ``FNO_CONTROL_PLANE_SCHEDULER`` (e.g. in the auto-continue launchd script)
to stamp rows emitted from a scheduled context with their real scheduler.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Optional

ARM_KING_WAKE = "king_wake"
ARM_WATCHDOG = "watchdog"
ARM_PR_WATCH_MERGE = "pr_watch_merge"
ARM_ACTIVE_BACKLOG = "active_backlog"
ARM_AUTO_CONTINUE = "auto_continue"
ARM_STOP_HOOK = "stop_hook"

EVENT_TYPE = "control_plane_tick"


def scheduler_from_env(default: str = "session") -> str:
    """The scheduler stamp for this process, or ``default`` when unset."""
    value = (os.environ.get("FNO_CONTROL_PLANE_SCHEDULER") or "").strip()
    return value or default


def emit_tick(
    arm: str,
    *,
    scheduler: str,
    interval_s: int,
    acted: int = 0,
    skip_reason: Optional[str] = None,
    detail: Optional[str] = None,
    events_path: Optional[Path] = None,
) -> bool:
    """Append one ``control_plane_tick`` row. Returns False (and never raises)
    when the write fails, so a readout row can never break the arm it
    observes."""
    if events_path is None:
        try:
            # FNO_EVENTS_PATH is the test pin (see fno.paths.project_events_json):
            # honoring it keeps an unpathed test emit out of the live journal.
            # Production arms write the global journal, where the readout scans.
            pin = os.environ.get("FNO_EVENTS_PATH")
            if pin:
                events_path = Path(pin)
            else:
                from fno.paths import state_dir

                events_path = state_dir() / "events.jsonl"
        except Exception as exc:  # noqa: BLE001 - a readout row must never break its arm
            import sys

            print(f"control_plane: journal path for {arm} unresolved: {exc}", file=sys.stderr)
            return False
    data: dict[str, Any] = {
        "arm": arm,
        "scheduler": scheduler,
        "acted": acted,
        "interval_s": int(interval_s),
    }
    if skip_reason is not None:
        data["skip_reason"] = skip_reason
    if detail is not None:
        data["detail"] = detail[:200]
    try:
        from fno.events import _build, append_event

        append_event(_build(EVENT_TYPE, "daemon", data), events_path)
        return True
    except Exception as exc:  # noqa: BLE001 - a readout row must never break its arm
        import sys

        print(f"control_plane: emit {arm} tick row failed: {exc}", file=sys.stderr)
        return False
