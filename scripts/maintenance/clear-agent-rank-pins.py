#!/usr/bin/env python3
"""Clear every rank left behind by the agent pins the fence now refuses.

Run once, at deploy. Before the fence, four callers pushed `rank --top` and
each write took `min(rank) - 1`, so the live ranks are a record of who ranked
last, not of what matters. The operator lane is empty, so every surviving pin
was written by an agent.

Reversible: `--apply` writes a restore file of every (id, rank) pair it
cleared, and `--restore <file>` puts them back. Dry-run is the default.

    python scripts/maintenance/clear-agent-rank-pins.py
    python scripts/maintenance/clear-agent-rank-pins.py --apply
    python scripts/maintenance/clear-agent-rank-pins.py --restore ranks-<ts>.json
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from datetime import datetime, timezone
from pathlib import Path


def _is_ranked(entry: dict) -> bool:
    rank = entry.get("rank")
    if isinstance(rank, bool) or not isinstance(rank, (int, float)):
        return False
    try:
        return math.isfinite(float(rank))
    except (OverflowError, ValueError):
        return False


def _is_open(entry: dict) -> bool:
    """Open and not deferred: what the board and every selector still read.

    A pin on a done or deferred row orders nothing, so clearing it would be
    churn in the graph for no change in behavior.
    """
    from fno.graph._reconcile import node_is_open

    return node_is_open(entry) and not entry.get("deferred_at")


def _pinned(entries: list[dict]) -> list[tuple[str, float]]:
    return [
        (entry["id"], float(entry["rank"]))
        for entry in entries
        if isinstance(entry, dict)
        and isinstance(entry.get("id"), str)
        and _is_ranked(entry)
        and _is_open(entry)
    ]


def _clear(targets: set[str]):
    def mutator(entries):
        for entry in entries:
            if isinstance(entry, dict) and entry.get("id") in targets:
                entry["rank"] = None
        return entries

    return mutator


def _restore(pairs: dict[str, float]):
    def mutator(entries):
        for entry in entries:
            if isinstance(entry, dict) and entry.get("id") in pairs:
                entry["rank"] = pairs[entry["id"]]
        return entries

    return mutator


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="write the change")
    parser.add_argument("--restore", type=Path, help="put a restore file's ranks back")
    args = parser.parse_args(argv)

    from fno.graph.cli import _graph_path
    from fno.graph.store import locked_mutate_graph, read_graph

    graph_path = _graph_path()

    if args.restore:
        pairs = {k: float(v) for k, v in json.loads(args.restore.read_text()).items()}
        locked_mutate_graph(graph_path, _restore(pairs))
        print(f"restored {len(pairs)} rank(s) from {args.restore}")
        return 0

    pinned = _pinned(read_graph(graph_path))
    if not pinned:
        print("no open non-deferred row carries a rank; nothing to clear")
        return 0

    for node_id, rank in sorted(pinned, key=lambda pair: pair[1]):
        print(f"  {node_id}  rank={rank}")
    print(f"{len(pinned)} pinned open row(s)")

    if not args.apply:
        print("dry run; pass --apply to clear them")
        return 0

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    restore_file = Path.cwd() / f"agent-rank-pins-{stamp}.json"
    restore_file.write_text(json.dumps(dict(pinned), indent=2))
    locked_mutate_graph(graph_path, _clear({node_id for node_id, _ in pinned}))
    print(f"cleared {len(pinned)} rank(s); restore file: {restore_file}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
