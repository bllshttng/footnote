"""Reap dead workers: soft-delete abandoned entries in workers.jsonl."""
from __future__ import annotations

from datetime import datetime, timezone, timedelta
from pathlib import Path

from fno.runtime.registry import read_workers, update_worker_status

_ABANDON_THRESHOLD_MINUTES = 30


def _is_abandoned(worker: dict, artifacts_dir: Path, threshold_minutes: int) -> bool:
    """Return True if a worker meets all abandonment criteria.

    Abandoned criteria (ALL must be true):
    1. status == "started"
    2. last_heartbeat is more than threshold_minutes ago
    3. No ship-{session_id}.md artifact exists in artifacts_dir
    """
    if worker.get("status") != "started":
        return False

    heartbeat_str = worker.get("last_heartbeat") or worker.get("started_at", "")
    if not heartbeat_str:
        return False

    try:
        # Parse ISO timestamp - handle both offset-aware and naive
        heartbeat = datetime.fromisoformat(heartbeat_str)
        if heartbeat.tzinfo is None:
            heartbeat = heartbeat.replace(tzinfo=timezone.utc)
    except ValueError:
        # Malformed timestamp - treat as stale. A worker with an unparseable
        # heartbeat is in unknown state; the prior "return False" (not stale)
        # created permanent zombie entries that reap never touched. Preferring
        # fail-closed: classify as abandoned so the operator sees it.
        import sys
        print(
            f"reap._is_abandoned: worker {worker.get('worker_id', '?')!r} has "
            f"unparseable timestamp {heartbeat_str!r}; treating as stale.",
            file=sys.stderr,
        )
        # Fall through to check for ship artifact - if present, worker is
        # actually completed and we shouldn't reap. Otherwise treat as abandoned.
        session_id = worker.get("session_id", "")
        if session_id:
            ship_artifact = artifacts_dir / f"ship-{session_id}.md"
            if ship_artifact.exists():
                return False
        return True

    now = datetime.now(timezone.utc)
    age_minutes = (now - heartbeat).total_seconds() / 60
    if age_minutes <= threshold_minutes:
        return False

    # Check for ship artifact
    session_id = worker.get("session_id", "")
    if session_id:
        ship_artifact = artifacts_dir / f"ship-{session_id}.md"
        if ship_artifact.exists():
            return False

    return True


def _classify_worker(worker: dict, artifacts_dir: Path, threshold_minutes: int) -> str:
    """Return 'abandoned', 'completed', or 'active' classification."""
    if worker.get("status") != "started":
        # Already processed - keep as-is
        return "other"

    heartbeat_str = worker.get("last_heartbeat") or worker.get("started_at", "")
    if heartbeat_str:
        try:
            heartbeat = datetime.fromisoformat(heartbeat_str)
            if heartbeat.tzinfo is None:
                heartbeat = heartbeat.replace(tzinfo=timezone.utc)
            now = datetime.now(timezone.utc)
            age_minutes = (now - heartbeat).total_seconds() / 60
            stale = age_minutes > threshold_minutes
        except ValueError:
            # Malformed timestamp - match _is_abandoned semantics: treat as stale.
            import sys
            print(
                f"reap._classify_worker: worker {worker.get('worker_id', '?')!r} "
                f"has unparseable timestamp {heartbeat_str!r}; classifying as stale.",
                file=sys.stderr,
            )
            stale = True
    else:
        stale = False

    session_id = worker.get("session_id", "")
    has_ship_artifact = bool(session_id and (artifacts_dir / f"ship-{session_id}.md").exists())

    if stale and has_ship_artifact:
        return "completed"
    if stale and not has_ship_artifact:
        return "abandoned"
    return "active"


def reap_dead_workers(
    *,
    workers_file: Path | None = None,
    artifacts_dir: Path | None = None,
    dry_run: bool = False,
    threshold_minutes: int = _ABANDON_THRESHOLD_MINUTES,
) -> dict:
    """Reap workers that meet the abandonment criteria.

    Abandoned = status=="started" AND last_heartbeat > threshold_minutes ago
                AND no ship-{session_id}.md artifact exists.

    Soft-delete: flips status to "abandoned" - never removes rows.

    Args:
        workers_file: Path to workers.jsonl. Defaults to .fno/workers.jsonl.
        artifacts_dir: Directory containing ship-{session_id}.md artifacts.
                       Defaults to .fno/artifacts/.
        dry_run: If True, compute report but do NOT mutate the file.
        threshold_minutes: Age threshold for abandonment (default 30).

    Returns:
        {"reaped": int, "active": int, "completed": int, "dry_run": bool}
    """
    if workers_file is None:
        from fno.runtime.registry import _default_workers_file
        workers_file = _default_workers_file()

    if artifacts_dir is None:
        artifacts_dir = workers_file.parent / "artifacts"

    workers = read_workers(workers_file=workers_file)

    counts = {"active": 0, "abandoned": 0, "completed": 0, "other": 0}
    to_reap: list[str] = []

    for worker in workers:
        classification = _classify_worker(worker, artifacts_dir, threshold_minutes)
        counts[classification] += 1
        if classification == "abandoned":
            to_reap.append(worker["worker_id"])

    if not dry_run:
        for worker_id in to_reap:
            update_worker_status(
                worker_id=worker_id,
                new_status="abandoned",
                workers_file=workers_file,
            )

    return {
        "reaped": len(to_reap),
        "active": counts["active"],
        "completed": counts["completed"],
        "dry_run": dry_run,
    }
