#!/usr/bin/env python3
"""Report ledger telemetry coverage: % of rows carrying termination_reason and
graph_node_id.

The finalize coverage gap's acceptance is measured, not asserted: terminal
sessions should reach 100% termination_reason coverage. This is the fold that
measures it (and the seed of a future scoreboard coverage line). Read-only.

Usage:
    python3 scripts/metrics/ledger-coverage.py              # last 7 days
    python3 scripts/metrics/ledger-coverage.py --days 30
    python3 scripts/metrics/ledger-coverage.py --days 0     # all rows
"""
from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path


def _entries(data: object) -> list[dict]:
    if isinstance(data, dict) and isinstance(data.get("entries"), list):
        return data["entries"]
    if isinstance(data, list):
        return data
    return []


def _row_ts(row: dict) -> str:
    return str(row.get("completed") or row.get("ts") or row.get("started") or "")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--ledger", type=Path, default=None)
    ap.add_argument("--days", type=int, default=7,
                    help="Window in days (0 = all rows). Default 7.")
    args = ap.parse_args()

    from fno import paths

    ledger_path = (args.ledger or paths.ledger_json()).resolve() if args.ledger \
        else paths.ledger_json()
    rows = _entries(json.loads(Path(ledger_path).read_text(encoding="utf-8")))

    cutoff = ""
    if args.days > 0:
        cutoff = (datetime.now(timezone.utc) - timedelta(days=args.days)).isoformat()[:10]

    # Terminal/ship rows are the ones that should carry the signals.
    ship = [
        r for r in rows
        if isinstance(r, dict)
        and (r.get("pr_number") or r.get("pr_url") or r.get("type") == "execution")
        and (not cutoff or _row_ts(r)[:10] >= cutoff)
    ]
    n = len(ship)
    if n == 0:
        print(f"no terminal/ship rows in window (days={args.days}).")
        return 0

    have_term = sum(1 for r in ship if r.get("termination_reason"))
    have_node = sum(
        1 for r in ship
        if r.get("graph_node_id") or r.get("node_id_unrecoverable")
    )
    print(f"ledger: {ledger_path}")
    print(f"window: {'all' if args.days == 0 else f'last {args.days}d'} "
          f"({n} terminal/ship rows)")
    print(f"  termination_reason: {have_term}/{n} ({100*have_term//n}%)")
    print(f"  node linkage:       {have_node}/{n} ({100*have_node//n}%)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
