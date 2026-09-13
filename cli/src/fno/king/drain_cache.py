"""Disk caches keyed on the graph file's stat identity (the keeper's own
FileIdent fields): the drain's per-scope count, and shared helpers. A changed
graph misses by construction, and every helper fails open - a cache that
cannot be read or written must never add a failure mode to a verb whose
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

    None too when the store names the sqlite backend in the sibling db's
    graph_meta: the keeper serves rows from graph.db and the json file can
    lag until an export, so a file-identity cache would never invalidate.
    ctime rides along: a same-size same-mtime overwrite moves neither, and
    only ctime catches it (the keeper's cache carries it for the same reason).
    """
    try:
        st = Path(path).stat()
        if _sqlite_backend(path):
            return None
    except OSError:
        return None
    return (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns)


def _sqlite_backend(path: Path) -> bool:
    """The keeper's own predicate, read-only: unset/absent db reads as json."""
    import sqlite3

    try:
        con = sqlite3.connect(f"file:{path.with_suffix('.db')}?mode=ro", uri=True)
        try:
            row = con.execute(
                "SELECT value FROM graph_meta WHERE key = 'backend'"
            ).fetchone()
        finally:
            con.close()
    except Exception:  # noqa: BLE001 - any read fault reads as the json default
        return False
    return bool(row) and row[0] == "sqlite"


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
    """Best-effort atomic write; concurrent drains lose a row to the last
    rename, never to corruption, and a failed write is silently dropped."""
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
