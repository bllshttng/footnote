#!/usr/bin/env python3
"""One-off: backfill graph_node_id on ship rows that lost it.

Epic x-f063 Wave 1 (ledger trust). Some shipped-session ledger rows carry a
null ``graph_node_id`` even though the node id survives in the row's ``title``
(e.g. ``ab-16daa753`` / ``x-2c17``) or ``branch`` (e.g. ``feature/x-2c17``),
which makes the scoreboard's node-level joins false-pessimistic.

For each SHIP row (has a pr_number / pr_url) missing ``graph_node_id`` and not
already stamped:

  - Extract node-id-shaped tokens from ``title`` then ``branch``.
  - Keep only tokens that resolve to EXACTLY ONE existing graph node (read-only
    against graph.json). The existence check is the disambiguation guard.
  - Exactly one distinct match  -> write ``graph_node_id`` (never guess).
  - Zero or more-than-one match -> stamp ``node_id_unrecoverable: true`` so the
    scoreboard reports coverage honestly instead of attributing to a guess.

Idempotent: a row already carrying ``graph_node_id`` or
``node_id_unrecoverable`` is skipped, so a re-run changes nothing. Writes are
atomic under the register flock (refuses on contention). Dry-run by default.

Usage:
    python3 scripts/metrics/backfill-ledger-node-id.py             # dry-run
    python3 scripts/metrics/backfill-ledger-node-id.py --apply     # write
    python3 scripts/metrics/backfill-ledger-node-id.py --ledger /path/ledger.json
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import re
import sys
import tempfile
from pathlib import Path

LEDGER_LOCK_PATH = Path("/tmp/abilities-ledger.lock")

# A node-id-shaped token: a short alpha(+num) prefix, a dash, and 4-8 hex.
# Covers the legacy ``ab-16daa753`` (8 hex) and the current ``x-2c17`` (4 hex),
# and any config-driven prefix; the exact-match-against-graph guard below
# rejects any coincidental hit that is not a real node.
_TOKEN = re.compile(r"\b([a-z][a-z0-9]{0,9}-[0-9a-f]{4,8})\b")


def _entries(data: object) -> list[dict]:
    if isinstance(data, dict) and isinstance(data.get("entries"), list):
        return data["entries"]
    if isinstance(data, list):
        return data
    return []


def _atomic_write_json(path: Path, data: object) -> None:
    directory = path.parent
    directory.mkdir(parents=True, exist_ok=True)
    tmp_path: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", dir=str(directory), prefix=f".{path.name}.",
            suffix=".tmp", delete=False, encoding="utf-8",
        ) as tmp:
            tmp_path = tmp.name
            tmp.write(json.dumps(data, indent=2))
            tmp.flush()
            os.fsync(tmp.fileno())
        os.replace(tmp_path, path)
        tmp_path = None
    finally:
        if tmp_path is not None:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass


def resolve_row(row: dict, node_ids: set[str]) -> tuple[str, str | None]:
    """Return (outcome, node_id).

    outcome is one of: 'title' / 'branch' (resolved, node_id set), or
    'unrecoverable' (node_id None). A token counts only when it is an existing
    graph node; exactly one distinct existing node across a field resolves it.
    """
    for field in ("title", "branch"):
        value = row.get(field)
        if not isinstance(value, str):
            continue
        matches = {t for t in _TOKEN.findall(value) if t in node_ids}
        if len(matches) == 1:
            return field, next(iter(matches))
        if len(matches) > 1:
            # Ambiguous within a single field: never guess (AC1-EDGE).
            return "unrecoverable", None
    return "unrecoverable", None


def backfill(ledger_path: Path, node_ids: set[str], apply: bool) -> int:
    data = json.loads(ledger_path.read_text(encoding="utf-8"))
    rows = _entries(data)

    counts = {"title": 0, "branch": 0, "unrecoverable": 0}
    changed = 0
    for row in rows:
        if not isinstance(row, dict):
            continue
        if row.get("graph_node_id") or row.get("node_id_unrecoverable"):
            continue  # idempotent: already resolved or already stamped
        if not (row.get("pr_number") or row.get("pr_url")):
            continue  # only ship rows need node linkage
        outcome, node_id = resolve_row(row, node_ids)
        counts[outcome] += 1
        changed += 1
        if node_id is not None:
            row["graph_node_id"] = node_id
        else:
            row["node_id_unrecoverable"] = True

    print(f"ship rows needing backfill: {changed}")
    print(f"  resolved from title:  {counts['title']}")
    print(f"  resolved from branch: {counts['branch']}")
    print(f"  unrecoverable:        {counts['unrecoverable']}")

    if not apply:
        print("\n[dry-run] pass --apply to write.")
        return 0
    if not changed:
        print("nothing to write.")
        return 0

    backup = ledger_path.with_suffix(".json.pre-nodeid-backfill.bak")
    backup.write_text(ledger_path.read_text(encoding="utf-8"), encoding="utf-8")
    _atomic_write_json(ledger_path, data)
    print(f"wrote {ledger_path} (backup: {backup})")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--ledger", type=Path, default=None,
                    help="Ledger path. Default: paths.ledger_json() (global).")
    ap.add_argument("--graph", type=Path, default=None,
                    help="Graph path. Default: paths.graph_json().")
    ap.add_argument("--apply", action="store_true", help="Write (default: dry-run).")
    args = ap.parse_args()

    from fno import paths

    ledger_path = args.ledger or paths.ledger_json()
    graph_path = args.graph or paths.graph_json()
    ledger_path = Path(ledger_path).resolve()

    graph = json.loads(Path(graph_path).read_text(encoding="utf-8"))
    node_ids = {
        e.get("id") for e in _entries(graph)
        if isinstance(e, dict) and e.get("id")
    }
    print(f"graph nodes: {len(node_ids)} | ledger: {ledger_path}")

    lock_fd = os.open(str(LEDGER_LOCK_PATH), os.O_CREAT | os.O_RDWR)
    try:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("ledger lock contended; refusing. Re-run when idle.", file=sys.stderr)
            return 2
        return backfill(ledger_path, node_ids, args.apply)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


if __name__ == "__main__":
    sys.exit(main())
