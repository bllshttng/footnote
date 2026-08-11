"""Retention-aware compaction for an events.jsonl journal."""

from __future__ import annotations

import json
import os
import tempfile
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fno.events import RETENTION_MINIMUM_TTL_HOURS, retention_for
from fno.mutex import acquire_dir_mutex, release_dir_mutex


def _timestamp(value: object) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None or parsed.utcoffset() != timedelta(0):
        return None
    return parsed.astimezone(timezone.utc)


def gc_events(
    events_path: Path,
    *,
    now: datetime | None = None,
    ttl_hours: int = RETENTION_MINIMUM_TTL_HOURS,
    dry_run: bool = False,
) -> dict[str, int]:
    """Delete only expired rows explicitly classified as ephemeral."""
    if ttl_hours < RETENTION_MINIMUM_TTL_HOURS:
        raise ValueError(
            "ttl_hours is shorter than the schema minimum retention horizon "
            f"({RETENTION_MINIMUM_TTL_HOURS} hours)"
        )
    reference = now or datetime.now(timezone.utc)
    if reference.tzinfo is None or reference.utcoffset() != timedelta(0):
        raise ValueError("now must be timezone-aware UTC")
    cutoff = reference - timedelta(hours=ttl_hours)

    path = Path(events_path).resolve()
    result = {"scanned": 0, "deleted": 0, "kept": 0, "malformed": 0}
    if not path.exists():
        return result

    gc_dir = path.with_name(path.name + ".gc.d")
    gc_token = acquire_dir_mutex(gc_dir, 30)
    if gc_token is None:
        raise TimeoutError(f"events.jsonl gc lock timeout: {gc_dir}")
    lock_dir = path.with_name(path.name + ".lock.d")
    lock_token: str | None = None
    try:
        # Shell writers check the GC marker before their unlocked append. Give
        # an append that passed the check immediately before marker creation a
        # bounded grace period to finish before the atomic rewrite.
        time.sleep(0.1)
        lock_token = acquire_dir_mutex(lock_dir, 30)
        if lock_token is None:
            raise TimeoutError(f"events.jsonl lock timeout: {lock_dir}")

        kept: list[str] = []
        for raw in path.read_text(encoding="utf-8").splitlines():
            if not raw.strip():
                kept.append(raw)
                continue
            result["scanned"] += 1
            try:
                event = json.loads(raw)
            except json.JSONDecodeError:
                result["malformed"] += 1
                result["kept"] += 1
                kept.append(raw)
                continue
            if not isinstance(event, dict):
                result["malformed"] += 1
                result["kept"] += 1
                kept.append(raw)
                continue
            event_type = event.get("type")
            timestamp = _timestamp(event.get("ts") or event.get("timestamp"))
            if not isinstance(event_type, str):
                result["malformed"] += 1
                result["kept"] += 1
                kept.append(raw)
                continue
            if retention_for(event_type) != "ephemeral" or timestamp is None:
                if timestamp is None:
                    result["malformed"] += 1
                result["kept"] += 1
                kept.append(raw)
                continue
            if timestamp < cutoff:
                result["deleted"] += 1
                continue
            result["kept"] += 1
            kept.append(raw)

        if not dry_run and result["deleted"]:
            path.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                dir=path.parent,
                prefix=f".{path.name}.gc-",
                delete=False,
            ) as handle:
                temp_path = Path(handle.name)
                handle.write("\n".join(kept))
                if kept:
                    handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
            try:
                temp_path.chmod(path.stat().st_mode)
                os.replace(temp_path, path)
            finally:
                temp_path.unlink(missing_ok=True)
        return result
    finally:
        if lock_token is not None:
            release_dir_mutex(lock_dir, lock_token)
        release_dir_mutex(gc_dir, gc_token)
