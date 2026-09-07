"""graph/fts.py - FTS5 search index as a HASH-VALIDATED CACHE.

The index is never a second source of truth. Its ``meta`` table stores the
sha256 of graph.json's bytes; every read hashes the graph (~6 ms on the
current graph) and compares. Any mismatch - a mutation, a reverted backup, a
hand-mangled file - triggers a FULL rebuild (~45 ms, ~9 MB) into a temp file
that atomically replaces the index. There is NO incremental write path, by
design: incremental sync means two writers, a drift window, and a stale index
that answers confidently. This codebase's most-repeated defect class is a
snapshot that lied; a hash cache key plus a disposable cache is the shape
that cannot lie - the worst case is a slow read, never a wrong one.

Stdlib sqlite3 only. FTS5 ships in CPython's bundled sqlite on the common
platforms; where it does not, :class:`SearchUnavailableError` lets the caller
degrade to the substring lane instead of failing.
"""
from __future__ import annotations

import hashlib
import os
import sqlite3
import tempfile
from datetime import datetime, timezone
from pathlib import Path

_SCHEMA = (
    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
    "CREATE VIRTUAL TABLE nodes USING fts5("
    "id UNINDEXED, title, slug, details, tokenize='unicode61');",
)


class SearchUnavailableError(RuntimeError):
    """FTS5 is missing from this Python's sqlite build."""


def index_path(graph_path: Path) -> Path:
    """The cache lives beside the graph."""
    return Path(str(graph_path) + ".fts5")


def _graph_hash(graph_path: Path) -> str:
    # A cache key, not a gate: the hash decides when to rebuild, never
    # whether the graph is readable.
    return hashlib.sha256(graph_path.read_bytes()).hexdigest()


def _fts5_supported() -> bool:
    try:
        conn = sqlite3.connect(":memory:")
        try:
            conn.execute("CREATE VIRTUAL TABLE _probe USING fts5(x)")
        finally:
            conn.close()
        return True
    except sqlite3.OperationalError:
        return False


def _stored_hash(path: Path) -> str | None:
    conn = sqlite3.connect(path)
    try:
        row = conn.execute("SELECT value FROM meta WHERE key = 'graph_sha256'").fetchone()
    finally:
        conn.close()
    return row[0] if row else None


def _rebuild(graph_path: Path, dest: Path, graph_hash: str) -> None:
    """Full rebuild into ``dest``. The ONLY writer of index bytes.

    Reads the graph through the reader seam (never raw json), so the index
    sees exactly what every other reader sees.
    """
    from fno.graph.store import read_graph

    entries = read_graph(graph_path)
    conn = sqlite3.connect(dest)
    try:
        for stmt in _SCHEMA:
            conn.execute(stmt)
        rows = []
        for e in entries:
            if not isinstance(e, dict) or not isinstance(e.get("id"), str):
                continue
            # A hand-mangled non-str field would fail the executemany bind
            # and degrade every search to the fallback forever; coerce here.
            rows.append(
                (
                    e["id"],
                    e.get("title") if isinstance(e.get("title"), str) else "",
                    e.get("slug") if isinstance(e.get("slug"), str) else "",
                    e.get("details") if isinstance(e.get("details"), str) else "",
                )
            )
        conn.executemany("INSERT INTO nodes (id, title, slug, details) VALUES (?,?,?,?)", rows)
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('graph_sha256', ?)",
            (graph_hash,),
        )
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('built_at', ?)",
            (datetime.now(timezone.utc).isoformat(),),
        )
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('node_count', ?)",
            (str(len(rows)),),
        )
        conn.commit()
    finally:
        conn.close()


def ensure_search_index(graph_path: Path) -> Path:
    """Return a current index for ``graph_path``, rebuilding when stale.

    Hash first, compare, and only then touch disk: a hit costs the hash. A
    miss (absent, corrupt, or stale index) rebuilds from scratch into a
    temp file in the same directory and atomically replaces the index, so a
    concurrent reader either sees the old complete index or the new one,
    never a partial. Concurrent rebuilders last-wins, each complete.
    """
    if not _fts5_supported():
        raise SearchUnavailableError(
            "FTS5 is unavailable in this Python's sqlite build; "
            "search falls back to substring matching"
        )
    if not graph_path.exists():
        raise FileNotFoundError(str(graph_path))
    graph_hash = _graph_hash(graph_path)
    path = index_path(graph_path)
    if path.exists():
        try:
            if _stored_hash(path) == graph_hash:
                return path  # hit: the cache is provably current
        except sqlite3.DatabaseError:
            pass  # corrupt cache = miss; rebuild over it
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=path.name, suffix=".tmp")
    os.close(fd)
    try:
        _rebuild(graph_path, Path(tmp), graph_hash)
        os.replace(tmp, str(path))
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return path


def search(query: str, graph_path: Path, limit: int | None = 20) -> list[str]:
    """Ranked node ids for ``query``; ensures the cache first.

    ``limit=None`` returns the full ranked set (the caller applies its own
    filters before truncating). Each whitespace token is double-quoted
    (internal quotes doubled) and joined by spaces, which FTS5 reads as
    implicit AND, so arbitrary prose cannot inject query syntax.

    The graph can be rewritten between the hash check and the query, which
    would answer from pre-mutation content. So the hash is re-read AFTER the
    query: a change forces one full retry, making the answer reflect a
    snapshot that was current when the read started (the same contract as a
    plain graph read, never a confidently stale one).
    """
    terms = [f'"{t.replace(chr(34), chr(34) * 2)}"' for t in query.split() if t]
    if not terms:
        return []
    match_sql = " ".join(terms)
    for attempt in (0, 1):
        path = ensure_search_index(graph_path)
        conn = sqlite3.connect(path)
        try:
            rows = conn.execute(
                "SELECT id FROM nodes WHERE nodes MATCH ? ORDER BY rank LIMIT ?",
                (match_sql, -1 if limit is None else limit),
            ).fetchall()
            answered_hash = _stored_hash(path)
        finally:
            conn.close()
        if attempt or answered_hash == _graph_hash(graph_path):
            return [r[0] for r in rows]
    return [r[0] for r in rows]  # unreachable; keeps mypy honest about rows
