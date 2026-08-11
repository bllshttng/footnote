"""Retention-aware compaction for an events.jsonl journal."""

from __future__ import annotations

import json
import os
import tempfile
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fno.events import RETENTION_MINIMUM_TTL_HOURS, _utc_timestamp, retention_for
from fno.mutex import acquire_dir_mutex, release_dir_mutex, renew_dir_mutex

_LEASE_RENEW_EVERY_S = 30


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
                os.kill(pid, 0)
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
    from fno.paths import global_events_json

    if path == global_events_json().resolve():
        raise ValueError(
            "refusing to compact the global daemon journal while its bounded "
            "Branch-B writer remains intentionally unlocked"
        )
    result = {"scanned": 0, "deleted": 0, "kept": 0, "malformed": 0}
    if not path.exists():
        return result

    gc_dir = path.with_name(path.name + ".gc.d")
    gc_token = acquire_dir_mutex(gc_dir, 30)
    if gc_token is None:
        raise TimeoutError(f"events.jsonl gc lock timeout: {gc_dir}")
    lock_dir = path.with_name(path.name + ".lock.d")
    lock_token: str | None = None
    cursor = path.parent / ".think-offer-cursor"
    cursor_lock_dir = cursor.with_name(cursor.name + ".lock.d")
    cursor_token: str | None = None
    try:
        # The marker stops new shell writers. A writer that passed its first
        # marker check must register and recheck before appending, so an empty
        # rendezvous proves no unlocked shell append can target the old inode.
        _wait_for_shell_writers(path, 30)
        lock_token = acquire_dir_mutex(lock_dir, 30)
        if lock_token is None:
            raise TimeoutError(f"events.jsonl lock timeout: {lock_dir}")
        cursor_token = acquire_dir_mutex(cursor_lock_dir, 30)
        if cursor_token is None:
            raise TimeoutError(f"event cursor lock timeout: {cursor_lock_dir}")
        _recover_cursor_pending(path, cursor)

        next_lease_renewal = time.monotonic() + _LEASE_RENEW_EVERY_S

        def renew_leases_if_due() -> None:
            nonlocal next_lease_renewal
            if time.monotonic() < next_lease_renewal:
                return
            if (
                not renew_dir_mutex(gc_dir, gc_token)
                or not renew_dir_mutex(lock_dir, lock_token)
                or not renew_dir_mutex(cursor_lock_dir, cursor_token)
            ):
                raise RuntimeError("events.jsonl GC mutex ownership was lost")
            next_lease_renewal = time.monotonic() + _LEASE_RENEW_EVERY_S

        cursor_exists = cursor.exists()
        old_cursor = _read_cursor(cursor) if cursor_exists else 0
        mapped_cursor = 0
        source_offset = 0
        kept: list[bytes] = []
        with path.open("rb") as source:
            for line in source:
                renew_leases_if_due()
                line_end = source_offset + len(line)
                raw_bytes = line.rstrip(b"\r\n")
                decode_failed = False
                try:
                    raw = raw_bytes.decode("utf-8")
                except UnicodeDecodeError:
                    raw = ""
                    decode_failed = True
                if not raw.strip():
                    if decode_failed:
                        result["scanned"] += 1
                        result["malformed"] += 1
                        result["kept"] += 1
                    kept.append(line)
                    if line_end <= old_cursor:
                        mapped_cursor += len(line)
                    source_offset = line_end
                    continue
                result["scanned"] += 1
                try:
                    event = json.loads(raw)
                except json.JSONDecodeError:
                    result["malformed"] += 1
                    result["kept"] += 1
                    kept.append(line)
                    if line_end <= old_cursor:
                        mapped_cursor += len(line)
                    source_offset = line_end
                    continue
                if not isinstance(event, dict):
                    result["malformed"] += 1
                    result["kept"] += 1
                    kept.append(line)
                    if line_end <= old_cursor:
                        mapped_cursor += len(line)
                    source_offset = line_end
                    continue
                event_type = event.get("type")
                timestamp = _timestamp(event.get("ts") or event.get("timestamp"))
                if not isinstance(event_type, str):
                    result["malformed"] += 1
                    result["kept"] += 1
                    kept.append(line)
                    if line_end <= old_cursor:
                        mapped_cursor += len(line)
                    source_offset = line_end
                    continue
                if retention_for(event_type) != "ephemeral" or timestamp is None:
                    if timestamp is None:
                        result["malformed"] += 1
                    result["kept"] += 1
                    kept.append(line)
                    if line_end <= old_cursor:
                        mapped_cursor += len(line)
                    source_offset = line_end
                    continue
                if timestamp < cutoff:
                    result["deleted"] += 1
                    source_offset = line_end
                    continue
                result["kept"] += 1
                kept.append(line)
                if line_end <= old_cursor:
                    mapped_cursor += len(line)
                source_offset = line_end

        if old_cursor > source_offset:
            mapped_cursor = 0

        if not dry_run and result["deleted"]:
            path.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="wb",
                dir=path.parent,
                prefix=f".{path.name}.gc-",
                delete=False,
            ) as handle:
                temp_path = Path(handle.name)
                for raw in kept:
                    handle.write(raw)
                    renew_leases_if_due()
                handle.flush()
                os.fsync(handle.fileno())
            try:
                renew_leases_if_due()
                temp_path.chmod(path.stat().st_mode)
                pending = cursor.with_name(cursor.name + ".gc-pending")
                if cursor_exists:
                    replacement = temp_path.stat()
                    _atomic_write(
                        pending,
                        json.dumps(
                            {
                                "device": replacement.st_dev,
                                "inode": replacement.st_ino,
                                "cursor": mapped_cursor,
                            },
                            separators=(",", ":"),
                        ).encode("ascii"),
                    )
                os.replace(temp_path, path)
                if cursor_exists:
                    _atomic_write(cursor, str(mapped_cursor).encode("ascii"))
                    pending.unlink(missing_ok=True)
            finally:
                temp_path.unlink(missing_ok=True)
        return result
    finally:
        if cursor_token is not None:
            release_dir_mutex(cursor_lock_dir, cursor_token)
        if lock_token is not None:
            release_dir_mutex(lock_dir, lock_token)
        release_dir_mutex(gc_dir, gc_token)
