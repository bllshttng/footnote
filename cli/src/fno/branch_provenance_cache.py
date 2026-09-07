"""Per-repo cache of the stranded sweep, for board rendering.

The pr-watch tick already classifies every worktree (``sweep()``) and then
throws the rows away after one log line. This module persists the
non-CLEAN rows to ``<repo>/.fno/branch-provenance.json`` so the Kanban
board can render branch provenance - which branches have no remote, how
many commits exist only on this disk, which have no PR - without any
render-time git. A branch that resolves to no backlog node is exactly the
interesting case and is cached with ``node: null``, never dropped.

Written every tick, never appended; safe to delete (the next tick
rewrites it). Reads fail open to ``[]`` so a missing or malformed file
only omits the board section.
"""

from __future__ import annotations

import json
import logging
import os
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from fno.worktree_stranded import Row

from fno.worktree_stranded import CLEAN

CACHE_RELPATH = ".fno/branch-provenance.json"


def cache_path(repo: Path) -> Path:
    return Path(repo) / CACHE_RELPATH


def write_cache(
    repo: Path,
    rows: "list[Row]",
    entries_by_id: Optional[dict] = None,
) -> bool:
    """Persist the non-CLEAN rows atomically; True when the file was written.

    ``entries_by_id`` supplies node titles when the caller has the graph
    loaded; without it, one local graph read is spent here. Any failure is
    logged and answered False - the cache is a display input and must never
    break the tick leg that writes it.
    """
    if entries_by_id is None:
        try:
            from fno.graph.store import read_graph_strict

            entries_by_id = {
                e.get("id"): e for e in read_graph_strict() if isinstance(e, dict) and e.get("id")
            }
        except Exception:  # noqa: BLE001 - titles are decoration; rows are the payload
            entries_by_id = {}

    out: list[dict] = []
    for row in rows:
        if row.klass == CLEAN:
            continue
        node_entry = entries_by_id.get(row.node) if row.node else None
        out.append(
            {
                "branch": row.facts.get("branch"),
                "node": row.node,
                "node_title": (node_entry or {}).get("title"),
                "klass": row.klass,
                "unpushed": row.unpushed,
                "has_remote": row.facts.get("has_remote"),
                "age": row.age,
                "pr_number": row.facts.get("pr_number"),
                "live": row.facts.get("live"),
                "path": row.facts.get("path"),
            }
        )

    target = cache_path(repo)
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp_fd, tmp_path = tempfile.mkstemp(dir=target.parent, suffix=".tmp")
        try:
            with os.fdopen(tmp_fd, "w", encoding="utf-8") as f:
                json.dump(out, f)
            os.replace(tmp_path, str(target))
        except Exception:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass
            raise
        return True
    except Exception as exc:  # noqa: BLE001 - display cache, never break the tick
        logging.getLogger(__name__).warning(
            "branch_provenance_cache: write failed for %s: %s", repo, exc
        )
        return False


def read_cache(repo: Path) -> list[dict]:
    """Cached rows, fail-open: any read or parse problem answers []."""
    try:
        data = json.loads(cache_path(repo).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    return data if isinstance(data, list) else []
