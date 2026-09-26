"""Offline readers for the pr-status cache rows.

The cache itself (the coalescing chokepoint, the stale serve, the
`--refresh` escape) is the Rust owner now: crates/fno-agents/src/pr_status/
cache.rs, answered through the `authorized-merge` status door. What survives
here is the offline row reading `fno.graph.board` renders from - no network,
ever. The full narrative lives in docs/architecture/pr-status-verdict.md.
"""

from __future__ import annotations

import json
import math
import os
from pathlib import Path
from typing import Optional


def cache_dir() -> Path:
    env = os.environ.get("FNO_PR_STATUS_CACHE_DIR")
    if env:
        return Path(env)
    from fno import paths

    return paths.state_dir() / "cache" / "pr-status"


def read_row(key: str) -> Optional[dict]:
    """The cached row for `key`, or None when absent/corrupt. A corrupt row
    reads as a miss, never as an error: the network read is the truth."""
    try:
        row = json.loads((cache_dir() / (key + ".json")).read_text(encoding="utf-8"))
        return row if isinstance(row, dict) else None
    except (OSError, json.JSONDecodeError):
        return None


def _row_paths_newest(slug_key: str, pr: str) -> list[Path]:
    """Every row file for (slug_key, pr), newest mtime first. No network."""
    candidates = []
    for candidate in cache_dir().glob(f"{slug_key}-{pr}-*.json"):
        try:
            candidates.append((candidate.stat().st_mtime, candidate))
        except OSError:
            continue  # a racing prune won; fewer candidates, not a crash
    return [p for _, p in sorted(candidates, reverse=True)]


def _rows_newest_first(slug_key: str, pr: str):
    """Every cached row for (slug_key, pr), newest mtime first. No network."""
    for candidate in _row_paths_newest(slug_key, pr):
        row = read_row(candidate.stem)
        if row is not None:
            yield row


def newest_row_offline(slug_key: str, pr: str) -> Optional[dict]:
    """The newest cached row for this PR, by mtime. No network, ever.

    Deliberately head-agnostic: render it as "as of <ts>", never as the
    current verdict (docs/architecture/pr-status-verdict.md).
    """
    return next(_rows_newest_first(slug_key, pr), None)


def finite_or_zero(value: object) -> float:
    """`value` as a finite float, 0.0 when absent, unparseable or not finite.

    Public because a cache row is read on more than one path and the guard
    has to travel with it (docs/architecture/pr-status-verdict.md).
    """
    try:
        v = float(value or 0)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return 0.0
    return v if math.isfinite(v) else 0.0
