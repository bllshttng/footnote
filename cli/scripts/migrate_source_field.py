#!/usr/bin/env python3
"""Migrate graph.json `source: "adopt"` rows to `source: "intake"`.

Idempotent: a second run on an already-migrated graph reports zero
changes and exits 0. `--dry-run` previews the diff without writing.
The script is invoked manually, once per machine. New intakes always
write `"intake"` directly via cli/src/fno/graph/_intake.py.

Concurrency: acquires the same sibling `<graph>.lock` flock that
`fno backlog intake` uses (derived from the graph path), so a concurrent
intake against the same graph cannot race the read-modify-write window.

Usage:
    uv run python cli/scripts/migrate_source_field.py /path/to/graph.json
    uv run python cli/scripts/migrate_source_field.py /path/to/graph.json --dry-run
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import sys
from pathlib import Path


# Sibling-lock derivation copied from fno.graph.store._graph_lock_path (kept in
# sync by hand rather than imported: this one-shot script runs standalone via
# `uv run python cli/scripts/...` and importing the package would pull in
# yaml + filelock at module-load time).
def _graph_lock_path(path: Path) -> Path:
    try:
        base = path.resolve()
    except (OSError, RuntimeError):  # symlink loop: ELOOP or RuntimeError by version
        base = path
    return Path(str(base) + ".lock")


def _print_diff(entries: list[dict]) -> int:
    """Print one line per node that would be rewritten. Return the count."""
    n = 0
    for e in entries:
        if e.get("source") == "adopt":
            print(f"  [dry-run] would rewrite {e.get('id')}: source adopt -> intake")
            n += 1
    return n


def _resolve_entries(data: dict, path: Path) -> tuple[str, list[dict]] | None:
    """Detect which key holds the entries list. Returns (key, entries) or None on error.

    Explicit key check (rather than `or` fall-through) so a malformed file
    with both `entries: []` and `nodes: [...]` does not silently pick the
    wrong one and migrate the unread side.
    """
    if "entries" in data:
        key, entries = "entries", data["entries"]
    elif "nodes" in data:
        key, entries = "nodes", data["nodes"]
    else:
        sys.stderr.write(
            f"Error: {path} has no 'entries' or 'nodes' list\n"
        )
        return None
    if not isinstance(entries, list):
        sys.stderr.write(f"Error: {path}'s '{key}' is not a list\n")
        return None
    return key, entries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("graph_path", help="Path to graph.json")
    parser.add_argument(
        "--dry-run", action="store_true",
        help="Print proposed changes without writing"
    )
    args = parser.parse_args()

    path = Path(args.graph_path).expanduser()
    if not path.exists():
        sys.stderr.write(f"Error: graph.json not found at {path}\n")
        return 1

    # Acquire the graph's sibling flock so a concurrent `fno backlog intake`
    # cannot land a write between our read and our rename.
    lock_path = _graph_lock_path(path)
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    fcntl.flock(lock_fd, fcntl.LOCK_EX)
    try:
        try:
            data = json.loads(path.read_text())
        except (json.JSONDecodeError, OSError) as e:
            sys.stderr.write(f"Error: could not parse {path}: {e}\n")
            return 1

        if not isinstance(data, dict):
            sys.stderr.write(f"Error: {path} is not a JSON object\n")
            return 1

        resolved = _resolve_entries(data, path)
        if resolved is None:
            return 1
        _, entries = resolved

        null_count = sum(1 for e in entries if e.get("source") is None)
        intake_count = sum(1 for e in entries if e.get("source") == "intake")

        if args.dry_run:
            n = _print_diff(entries)
            print(f"Dry-run: would rewrite {n} node(s); "
                  f"{intake_count} unchanged; {null_count} had null source")
            return 0

        rewrote = 0
        for e in entries:
            if e.get("source") == "adopt":
                e["source"] = "intake"
                rewrote += 1

        # Atomic write via tempfile + rename. Wrap in try/except so an
        # OSError mid-write does not leak the .tmp file.
        tmp = path.with_suffix(path.suffix + ".tmp")
        try:
            tmp.write_text(json.dumps(data, indent=2))
            tmp.replace(path)
        except OSError as e:
            sys.stderr.write(f"Error: could not write {path}: {e}\n")
            try:
                tmp.unlink()
            except OSError:
                pass
            return 1

        print(f"Rewrote {rewrote} node(s); {intake_count} unchanged; "
              f"{null_count} had null source")
        return 0
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


if __name__ == "__main__":
    sys.exit(main())
