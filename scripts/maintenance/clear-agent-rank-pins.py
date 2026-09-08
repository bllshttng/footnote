#!/usr/bin/env python3
"""Clear EVERY rank in the graph, once, when the operator-only fence deploys.

Read the name as intent, not as a filter. `rank` stores no writer, so nothing
in the data distinguishes an agent pin from an operator one; this clears the
lot. That is the honest reading of the deploy: before the fence, callers
pushed `rank --top` and each write took `min(rank) - 1`, so the surviving
ranks record who ranked last rather than what matters, and an operator who
wants one back re-pins it deliberately.

Reversible. `--apply` writes a restore file of every (id, rank) pair it
cleared and `--restore <file>` puts them back. A rank that changed between the
preview and the locked write is left alone and named, because the restore file
would otherwise promise a recovery it cannot deliver. Dry run is the default.

    uv run --project cli python scripts/maintenance/clear-agent-rank-pins.py
    uv run --project cli python scripts/maintenance/clear-agent-rank-pins.py --apply
    uv run --project cli python scripts/maintenance/clear-agent-rank-pins.py --restore ./agent-rank-pins-<ts>.json
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from datetime import datetime, timezone
from pathlib import Path

# Run-directly bootstrap: make `fno.*` importable without an install, the same
# way the sibling migration in this directory does. Without it the invocation
# in the docstring above dies on ModuleNotFoundError.
_SRC = Path(__file__).resolve().parents[2] / "cli" / "src"
if _SRC.is_dir() and str(_SRC) not in sys.path:
    sys.path.insert(0, str(_SRC))


def _is_ranked(entry: dict) -> bool:
    rank = entry.get("rank")
    if isinstance(rank, bool) or not isinstance(rank, (int, float)):
        return False
    try:
        return math.isfinite(float(rank))
    except (OverflowError, ValueError):
        return False


def _pinned(entries: list[dict]) -> list[tuple[str, float]]:
    """Every finite rank in the graph, terminal and deferred rows included.

    Not just today's board. Nothing clears ``rank`` on closure or on a defer,
    so a pin left on a done row comes back the moment somebody reopens or
    undefers it, and by then no agent can clear it.
    """
    return [
        (entry["id"], float(entry["rank"]))
        for entry in entries
        if isinstance(entry, dict)
        and isinstance(entry.get("id"), str)
        and _is_ranked(entry)
    ]


def _clear(seen: dict[str, float]):
    """Clear only the ranks that still hold the value the restore file records.

    The selection runs again inside the lock. An operator pin written between
    the preview read and this mutation is left alone and named, because the
    restore file would otherwise promise a recovery it cannot deliver.
    """
    skipped: list[str] = []

    def mutator(entries):
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            node_id = entry.get("id")
            if node_id not in seen or not _is_ranked(entry):
                continue
            if float(entry["rank"]) != seen[node_id]:
                skipped.append(node_id)
                continue
            entry["rank"] = None
        return entries

    return mutator, skipped


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
        print("no row carries a rank; nothing to clear")
        return 0

    for node_id, rank in sorted(pinned, key=lambda pair: pair[1]):
        print(f"  {node_id}  rank={rank}")
    print(f"{len(pinned)} pinned row(s)")

    if not args.apply:
        print("dry run; pass --apply to clear them")
        return 0

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    restore_file = Path.cwd() / f"agent-rank-pins-{stamp}.json"
    restore_file.write_text(json.dumps(dict(pinned), indent=2))
    mutator, skipped = _clear(dict(pinned))
    locked_mutate_graph(graph_path, mutator)
    print(f"cleared {len(pinned) - len(skipped)} rank(s); restore file: {restore_file}")
    for node_id in skipped:
        print(f"  left alone: {node_id} was re-ranked after the preview read")
    return 0


if __name__ == "__main__":
    sys.exit(main())
