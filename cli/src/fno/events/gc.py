"""Retention-aware compaction for an events.jsonl journal."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fno.events import (
    RETENTION_MINIMUM_TTL_HOURS,
    ValidationError,
    _utc_timestamp,
    retention_for,
    validate,
)


def gc_events(
    events_path: Path,
    *,
    now: datetime | None = None,
    ttl_hours: int = RETENTION_MINIMUM_TTL_HOURS,
    dry_run: bool = False,
) -> dict[str, int]:
    """Delete only expired ephemeral rows, now from the SQL store.

    The store owns the retention classes, so the file-rewrite machinery this
    function once ran (mkdir mutex, gc marker, line rewrite) is retired: one
    bounded SQL delete, and `durable`/`gate`/rejected rows never leave.
    The result shape is unchanged for the CLI fold.
    """
    if ttl_hours < RETENTION_MINIMUM_TTL_HOURS:
        raise ValueError(
            "ttl_hours is shorter than the schema minimum retention horizon "
            f"({RETENTION_MINIMUM_TTL_HOURS} hours)"
        )
    from fno.events.store_client import gc_ephemeral
    from fno.paths import global_events_json

    if Path(events_path).resolve() == global_events_json().resolve():
        raise ValueError(
            "refusing to compact the global daemon journal: it is shared "
            "across projects, so one project's TTL policy must not rewrite it"
        )
    reference = now or datetime.now(timezone.utc)
    now_ms = int(reference.timestamp() * 1000)
    result = gc_ephemeral(
        Path(events_path), ttl_hours=ttl_hours, dry_run=dry_run, now_ms=now_ms
    )
    if dry_run:
        return {
            "scanned": result["scanned"],
            "deleted": 0,
            "kept": result["scanned"],
            "malformed": result["malformed"],
        }
    return {
        "scanned": result["scanned"],
        "deleted": result["deleted"],
        "kept": result["kept"],
        "malformed": result["malformed"],
    }

