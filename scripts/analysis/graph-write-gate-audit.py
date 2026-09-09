#!/usr/bin/env python3
"""Merge graph-write-gate windows and enforce the predeclared SQLite port bar."""

from __ import annotations

import json
import sys
from pathlib import Path


def _events(value: object) -> list[dict]:
    if isinstance(value, list):
        return [row for row in value if isinstance(row, dict)]
    if isinstance(value, dict):
        for key in ("events", "rows", "items"):
            rows = value.get(key)
            if isinstance(rows, list):
                return [row for row in rows if isinstance(row, dict)]
    return []


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: graph-write-gate-audit.py EVENTS.json", file=sys.stderr)
        return 2
    try:
        rows = _events(json.loads(Path(sys.argv[1]).read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"graph write gate: unmeasured ({exc})")
        return 2
    windows = [row.get("data", row) for row in rows if row.get("type") in (None, "graph_write_gate")]
    windows = [row for row in windows if isinstance(row, dict)]
    if not windows or any(float(row.get("completed_window_seconds", 0)) < 295 for row in windows):
        print("graph write gate: unmeasured (no complete five-minute windows)")
        return 2
    bounds = windows[0].get("wait_ms_bounds")
    if not isinstance(bounds, list) or any(row.get("wait_ms_bounds") != bounds for row in windows):
        print("graph write gate: unmeasured (incompatible histogram bounds)")
        return 2
    counts = [0] * len(bounds)
    for row in windows:
        bucket_counts = row.get("wait_ms_counts")
        if not isinstance(bucket_counts, list) or len(bucket_counts) != len(bounds):
            print("graph write gate: unmeasured (malformed histogram)")
            return 2
        counts = [left + int(right) for left, right in zip(counts, bucket_counts)]
    total = sum(counts)
    if total == 0:
        print("graph write gate: unmeasured (zero mutations)")
        return 2
    threshold = total * 0.95
    seen = 0
    p95: float | str = "inf"
    for bound, count in zip(bounds, counts):
        seen += count
        if seen >= threshold:
            p95 = bound
            break
    sustained = max(float(row["mutation_count"]) * 60 / float(row["completed_window_seconds"]) for row in windows)
    passes = p95 == "inf" or float(p95) > 1000 or sustained > 60
    print(f"graph write gate: p95_wait_ms={p95} sustained_mutations_per_minute={sustained:.2f} windows={len(windows)} port_bar_met={str(passes).lower()}")
    return 0 if passes else 1


if __name__ == "__main__":
    raise SystemExit(main())
