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
from fno.mutex import acquire_dir_mutex, release_dir_mutex, renew_dir_mutex

_LEASE_RENEW_EVERY_S = 30


def _process_identity(pid: int) -> str | None:
    try:
        result = subprocess.run(
            ["ps", "-o", "lstart=", "-p", str(pid)],
            capture_output=True,
            text=True,
            check=False,
            env={**os.environ, "LC_ALL": "C", "LANG": "C"},
        )
    except OSError:
        return None
    value = " ".join(result.stdout.split())
    return value or None


def _timestamp(value: object) -> datetime | None:
    return _utc_timestamp(value)


def _wait_for_shell_writers(path: Path, timeout_seconds: float) -> None:
    active_dir = path.with_name(path.name + ".shell-writers.d")
    deadline = time.monotonic() + timeout_seconds
    while True:
        try:
            entries = list(active_dir.iterdir())
        except FileNotFoundError:
            return
        except OSError as exc:
            raise OSError(f"cannot inspect shell writer rendezvous {active_dir}: {exc}") from exc
        if not entries:
            try:
                active_dir.rmdir()
            except OSError:
                pass
            return
        for entry in entries:
            try:
                pid = int(entry.name.split(".", 1)[0])
                recorded_identity = (entry / "owner").read_text(encoding="utf-8").strip()
                current_identity = _process_identity(pid)
                if current_identity is None:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        (entry / "owner").unlink(missing_ok=True)
                        entry.rmdir()
                    except OSError:
                        pass
                    continue
                if current_identity == recorded_identity:
                    continue
                (entry / "owner").unlink(missing_ok=True)
                entry.rmdir()
            except FileNotFoundError:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    try:
                        entry.rmdir()
                    except OSError:
                        pass
            except ProcessLookupError:
                try:
                    entry.rmdir()
                except OSError:
                    pass
            except (OSError, ValueError, OverflowError):
                pass
        if time.monotonic() >= deadline:
            raise TimeoutError(f"shell writer rendezvous timeout: {active_dir}")
        time.sleep(0.05)


def _read_cursor(path: Path) -> int:
    try:
        value = int(path.read_text(encoding="ascii").strip())
    except (OSError, UnicodeError, ValueError):
        return 0
    return max(value, 0)


def _fanout_cursor_timestamps(status_dir: Path) -> set[datetime]:
    """Return timestamps whose same-second occurrence indexes must stay stable."""
    timestamps: set[datetime] = set()
    try:
        cursor_paths = list(status_dir.glob("*.cursor"))
    except OSError:
        return timestamps
    for cursor_path in cursor_paths:
        try:
            payload = json.loads(cursor_path.read_text(encoding="utf-8"))
            timestamp = payload["ts"]
            count = payload["n"]
        except (KeyError, OSError, TypeError, UnicodeError, ValueError):
            continue
        parsed = _timestamp(timestamp)
        if parsed is not None and isinstance(count, int) and not isinstance(count, bool) and count >= 0:
            timestamps.add(parsed)
    return timestamps


def _atomic_write(path: Path, payload: bytes) -> None:
    with tempfile.NamedTemporaryFile(
        mode="wb", dir=path.parent, prefix=f".{path.name}.", delete=False
    ) as handle:
        temp_path = Path(handle.name)
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    try:
        os.replace(temp_path, path)
    finally:
        temp_path.unlink(missing_ok=True)


def _recover_cursor_pending(path: Path, cursor: Path) -> None:
    pending = cursor.with_name(cursor.name + ".gc-pending")
    try:
        payload = json.loads(pending.read_text(encoding="ascii"))
    except FileNotFoundError:
        return
    stat = path.stat()
    if payload.get("device") != stat.st_dev or payload.get("inode") != stat.st_ino:
        pending.unlink(missing_ok=True)
        return
    value = payload.get("cursor")
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"invalid pending event cursor: {pending}")
    _atomic_write(cursor, str(value).encode("ascii"))
    pending.unlink()


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

    result = gc_ephemeral(Path(events_path), ttl_hours=ttl_hours, dry_run=dry_run)
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

