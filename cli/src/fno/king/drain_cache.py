"""Disk cache for `fno agents king drain`: one count per (scope, graph identity).

The stop gate shells the drain on every fire; on an unchanged graph each fire
re-paid the full keeper read only to count the same scope again, and under
fleet load that read outruns the gate's own STOPGATE_READ_TIMEOUT (read_bounds.rs).
Rows key on the graph file's stat identity, the keeper's own FileIdent fields,
so a changed graph misses by construction. Every helper fails open - a cache
that cannot be read or written must never add a failure mode to a verb whose
contract is exit 1 on an unreadable graph.
"""

from __future__ import annotations

import json
import os
from pathlib import Path


def cache_file() -> Path:
    from fno import paths

    return paths.state_dir() / "cache" / "king-drain.json"


def graph_ident(path: Path) -> "tuple | None":
    """Stat identity of the graph file, or None when it cannot be stat'd.

    ctime rides along because a same-size same-mtime overwrite in place
    moves neither, and only ctime catches it (the keeper's cache carries
    the same field for the same reason).
    """
    try:
        st = Path(path).stat()
    except OSError:
        return None
    return (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns)


def load(scope: str, ident: tuple) -> "int | None":
    """The cached count for `scope` when the graph still has this identity."""
    try:
        data = json.loads(cache_file().read_text(encoding="utf-8"))
        row = data.get(scope) if isinstance(data, dict) else None
        if isinstance(row, dict) and row.get("ident") == list(ident):
            return int(row["undelivered"])
    except (OSError, ValueError, TypeError, KeyError):
        pass
    return None


def store(scope: str, ident: tuple, undelivered: int) -> None:
    """Best-effort write; a failed cache write is silently dropped.

    Concurrent drains lose one row to the last rename, never to corruption:
    each writer replaces atomically, and both computed from the same
    identity, so either row is correct.
    """
    try:
        path = cache_file()
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            if not isinstance(data, dict):
                data = {}
        except (OSError, ValueError):
            data = {}
        data[scope] = {"ident": list(ident), "undelivered": int(undelivered)}
        tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, path)
    except OSError:
        pass
