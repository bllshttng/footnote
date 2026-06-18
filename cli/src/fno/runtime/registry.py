"""Worker registry: append-only JSONL file with filelock-protected writes."""
from __future__ import annotations

import json
import uuid
from datetime import datetime, timezone
from pathlib import Path

from filelock import FileLock

_DEFAULT_WORKERS_FILE = Path(".fno") / "workers.jsonl"


def register_worker(
    *,
    worker_id: str,
    task: str,
    campaign: str = "",
    session_id: str = "",
    pid: int | None = None,
    workers_file: Path | None = None,
) -> dict:
    """Append a new worker entry to the workers JSONL registry.

    The file is created if it does not exist. Writes are protected by a
    filelock so concurrent calls don't interleave bytes.

    Args:
        worker_id: Unique identifier for this worker.
        task: Human-readable task description.
        campaign: Optional campaign/plan identifier.
        session_id: Optional session identifier.
        pid: Optional process ID of the spawned subprocess.
        workers_file: Path to workers.jsonl. Defaults to .fno/workers.jsonl.

    Returns:
        The entry dict that was written.
    """
    if workers_file is None:
        workers_file = _DEFAULT_WORKERS_FILE

    workers_file.parent.mkdir(parents=True, exist_ok=True)

    entry: dict = {
        "worker_id": worker_id,
        "task": task,
        "campaign": campaign,
        "session_id": session_id,
        "status": "started",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "last_heartbeat": datetime.now(timezone.utc).isoformat(),
    }
    if pid is not None:
        entry["pid"] = pid

    lock_path = workers_file.with_suffix(".jsonl.lock")
    with FileLock(str(lock_path)):
        with open(workers_file, "a") as f:
            f.write(json.dumps(entry) + "\n")

    return entry


def read_workers(workers_file: Path | None = None) -> list[dict]:
    """Read all entries from the workers registry.

    Args:
        workers_file: Path to workers.jsonl. Defaults to .fno/workers.jsonl.

    Returns:
        List of worker entry dicts (all rows, including abandoned/completed).
    """
    if workers_file is None:
        workers_file = _DEFAULT_WORKERS_FILE

    if not workers_file.exists():
        return []

    entries = []
    for lineno, line in enumerate(workers_file.read_text().splitlines(), start=1):
        line = line.strip()
        if not line:
            continue
        try:
            entries.append(json.loads(line))
        except json.JSONDecodeError as exc:
            # One corrupt row should not brick the registry. Skip with a
            # diagnostic identifying the file + line so operators can repair.
            import sys
            print(
                f"registry.read_workers: skipping malformed JSON at "
                f"{workers_file}:{lineno}: {exc.msg}",
                file=sys.stderr,
            )
            continue
    return entries


def update_worker_status(
    *,
    worker_id: str,
    new_status: str,
    workers_file: Path | None = None,
) -> bool:
    """Update the status field of a specific worker (soft update via rewrite).

    The JSONL is append-only for new entries, but status updates rewrite
    the file under a filelock. This preserves audit history (no rows deleted)
    while allowing status mutation for reap purposes.

    Returns:
        True if the worker was found and updated, False otherwise.
    """
    if workers_file is None:
        workers_file = _DEFAULT_WORKERS_FILE

    if not workers_file.exists():
        return False

    lock_path = workers_file.with_suffix(".jsonl.lock")
    with FileLock(str(lock_path)):
        lines = workers_file.read_text().splitlines()
        new_lines = []
        found = False
        for lineno, line in enumerate(lines, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError as exc:
                # Preserve malformed lines verbatim so repair is possible;
                # log but do not crash the status update.
                import sys
                print(
                    f"registry.update_worker_status: preserving malformed "
                    f"JSON at {workers_file}:{lineno}: {exc.msg}",
                    file=sys.stderr,
                )
                new_lines.append(line)
                continue
            if entry.get("worker_id") == worker_id:
                entry["status"] = new_status
                found = True
            new_lines.append(json.dumps(entry))

        if found:
            workers_file.write_text("\n".join(new_lines) + "\n")

    return found
