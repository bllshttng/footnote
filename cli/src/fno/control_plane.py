"""Python emitter for ``control_plane_tick`` rows.

Shape owned by ``crates/fno-agents/src/tick_ledger.rs``, pinned by
``cli/src/fno/events/schema.yaml``; rows land in the global journal the arms
readout scans. ``FNO_CONTROL_PLANE_SCHEDULER`` stamps a scheduled context.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Any, Optional

from fno.events import _build, append_event
from fno.paths import state_dir

EVENT_TYPE = "control_plane_tick"


def scheduler_from_env(default: str = "session") -> str:
    """The scheduler stamp for this process, or ``default`` when unset."""
    value = (os.environ.get("FNO_CONTROL_PLANE_SCHEDULER") or "").strip()
    return value or default


def emit_tick(arm: str, *, scheduler: str, interval_s: int, acted: int = 0,
              skip_reason: Optional[str] = None, detail: Optional[str] = None,
              events_path: Optional[Path] = None) -> bool:
    """Append one tick row. Returns False, never raises, on a failed write: a
    readout row must never break the arm it observes."""
    try:
        if events_path is None:
            pin = os.environ.get("FNO_EVENTS_PATH")  # test pin
            events_path = Path(pin) if pin else state_dir() / "events.jsonl"
        data: dict[str, Any] = {"arm": arm, "scheduler": scheduler,
                                "acted": acted, "interval_s": int(interval_s)}
        if skip_reason is not None:
            data["skip_reason"] = skip_reason
        if detail is not None:
            data["detail"] = detail[:200]
        append_event(_build(EVENT_TYPE, "daemon", data), events_path)
        return True
    except Exception as exc:  # noqa: BLE001 - a readout row must never break its arm
        print(f"control_plane: emit {arm} tick row failed: {exc}", file=sys.stderr)
        return False
