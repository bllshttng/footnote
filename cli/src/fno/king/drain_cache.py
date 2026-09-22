"""Disk caches keyed on the store's own identity. A changed graph misses by
construction, and every helper fails open - a cache must never add a failure
mode to a verb whose contract is exit 1 on an unreadable graph.
"""

from __future__ import annotations

import json
import os
from pathlib import Path


def cache_file() -> Path:
    from fno import paths

    return paths.state_dir() / "cache" / "king-drain.json"


def graph_ident(path: Path) -> "tuple | None":
    """The store's identity for the rows it serves: json keys on the file's
    stat (the file IS the store; ctime catches a same-size same-mtime
    overwrite), sqlite on the store version (the file lags until export).
    Backend-tagged so a flip invalidates; no keeper, no identity."""
    try:
        from fno.graph.store import store_export_status
        status = store_export_status(Path(path))
        if not status:
            return None
        if status.get("backend") == "sqlite":
            version = status.get("version")
            return ("sqlite", version) if version else None
        st = Path(path).stat()
    except OSError:
        return None
    return ("json", st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns)


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
    rename, never to corruption; a failed write is silently dropped."""
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
