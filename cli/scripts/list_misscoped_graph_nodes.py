#!/usr/bin/env python3
"""List (or emit fixes for) graph nodes whose project/cwd drifted.

Detection is shared with ``fno backlog maintain`` (the re-scope leg) via
``fno.graph.maintain.detect_rescope_fixes``, so this diagnostic and the
ritual never disagree. It catches three drift shapes: a node whose ``project``
maps to a known workspace but whose ``cwd`` is not that workspace (a worktree
path), a node whose ``project`` name is unknown but whose ``cwd`` maps to a
known project, and a ``project: null`` node whose cwd (or conductor worktree
``<repo>`` segment) maps to a known project.

Read-only by default: prints a markdown table; emits no mutation.

    python cli/scripts/list_misscoped_graph_nodes.py            # report (table)
    python cli/scripts/list_misscoped_graph_nodes.py --apply    # emit `fno backlog update` lines
    python cli/scripts/list_misscoped_graph_nodes.py --apply | sh   # apply them

``--apply`` emits one ``fno backlog update <id> --project <p> --cwd <c>`` line
per drifted node (touching only project/cwd, never priority/status); it does NOT
run them itself, so the operator stays in control.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Make the fno package importable when run as a bare script (no install).
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from fno.graph.maintain import detect_rescope_fixes, load_workspaces  # noqa: E402


def _load_graph() -> list[dict]:
    graph_path = Path.home() / ".fno" / "graph.json"
    if not graph_path.exists():
        return []
    try:
        data = json.loads(graph_path.read_text())
    except (OSError, ValueError) as e:
        sys.stderr.write(
            f"warning: could not read {graph_path}: {e} - "
            "diagnostic may be incomplete\n"
        )
        return []
    return data.get("entries") or data.get("nodes") or []


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Emit `fno backlog update` command lines instead of a report table.",
    )
    args = parser.parse_args()

    workspaces = load_workspaces()
    entries = _load_graph()
    fixes = detect_rescope_fixes(entries, workspaces)

    if not fixes:
        if not args.apply:
            print("No misscoped nodes found.")
        return 0

    if args.apply:
        for f in fixes:
            print(
                f"fno backlog update {f.node_id} "
                f"--project {f.new_project} --cwd {f.new_cwd}"
            )
        return 0

    print("| id | current_project | cwd | expected_project | expected_cwd |")
    print("|---|---|---|---|---|")
    for f in fixes:
        print(
            f"| {f.node_id} | {f.old_project} | {f.old_cwd} | "
            f"{f.new_project} | {f.new_cwd} |"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
