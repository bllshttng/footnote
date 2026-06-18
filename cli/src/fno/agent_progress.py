"""Reader helpers for per-worktree agent-progress.jsonl files.

Consumed by the megawalk walker (phase 04) to check agent liveness and
surface recent progress in 'fno megawalk status'.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone, timedelta
from pathlib import Path


def read_progress(worktree_path: Path, *, last_n: int = 10) -> list[dict]:
    """Read the last N entries from a worktree's agent-progress.jsonl.

    Returns an empty list if the file is missing or unreadable.
    """
    path = worktree_path / ".fno" / "agent-progress.jsonl"
    if not path.exists():
        return []
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
        return [json.loads(line) for line in lines[-last_n:] if line.strip()]
    except (OSError, json.JSONDecodeError):
        return []


def latest_entry(worktree_path: Path) -> dict | None:
    """Return the single most recent entry, or None if file is missing/empty."""
    entries = read_progress(worktree_path, last_n=1)
    return entries[0] if entries else None


def newest_ts_within(worktree_path: Path, minutes: int) -> bool:
    """Return True if any of the last 20 entries has a ts within the last N minutes."""
    entries = read_progress(worktree_path, last_n=20)
    if not entries:
        return False
    now = datetime.now(timezone.utc)
    cutoff = now - timedelta(minutes=minutes)
    for entry in entries:
        ts_str = entry.get("ts", "")
        if not ts_str:
            continue
        try:
            # Handle both '+00:00' and 'Z' suffixes
            ts_str_normalized = ts_str.replace("Z", "+00:00")
            ts = datetime.fromisoformat(ts_str_normalized)
            if ts >= cutoff:
                return True
        except ValueError:
            continue
    return False
