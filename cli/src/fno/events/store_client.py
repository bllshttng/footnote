"""The Python side of the authoritative event store: a thin client of the
native ``fno doctor event`` storage verbs.

Python never opens an event journal for append. Every write rides the native
binary's SQL transaction (WAL, FULL sync, positive readback), so one commit
protocol serves every language. Best-effort callers keep their own policy;
this client never reports a committed event when SQL failed, and there is no
JSONL fallback.
"""
from __future__ import annotations

import json
import os
import shutil
import stat as stat_module
import subprocess
from pathlib import Path
from typing import Any, Optional


class EventStoreUnavailable(RuntimeError):
    """The native store could not commit the event. No fallback exists."""


def resolve_native_bin() -> str:
    """Resolve the native ``fno`` binary: ``FNO_BIN`` first (the hermetic
    runner passthrough, same channel as ``FNO_AGENTS_BIN``), then this
    checkout's own build, then ``PATH``. The checkout build outranks ``PATH``
    so a test tree never answers through a stale installed binary. A missing
    binary is a named failure, never a silent file write."""
    bin_path = os.environ.get("FNO_BIN")
    if bin_path:
        return bin_path
    # .../cli/src/fno/events/store_client.py -> parents[4] is the repo root.
    repo_root = Path(__file__).resolve().parents[4]
    for profile in ("debug", "release"):
        candidate = repo_root / "crates" / "fno" / "target" / profile / "fno"
        if candidate.exists():
            return str(candidate)
    found = shutil.which("fno")
    if found:
        return found
    raise EventStoreUnavailable(
        "no native fno binary on PATH; the event store has no fallback writer"
    )


def store_db_path(events_path: Path) -> Path:
    """The store beside a journal: symlinks resolved, rotation suffixes and
    the ``.jsonl`` stem stripped, ``.db`` appended - the same resolution the
    native store performs, so a locator names one store from either side."""
    resolved = Path(events_path).resolve()
    stem = resolved.name
    if stem.endswith(".jsonl"):
        stem = stem[: -len(".jsonl")]
    # A generation suffix (.1, .2) names the same store as the live journal.
    if stem.rsplit(".", 1)[-1].isdigit():
        stem = stem.rsplit(".", 1)[0]
    return resolved.with_name(f"{stem}.db")


def native_rows(
    events_path: Path,
    *,
    types: Optional[list[str]] = None,
    include_rejected: bool = False,
    timeout: float = 30,
) -> Optional[list[str]]:
    """One native read pass: import, then committed envelope lines.

    The whole reader contract lives in the binary; this is the transport.
    None means the native side was unavailable and the caller falls back to
    its legacy raw-journal behavior.
    """
    try:
        bin_path = resolve_native_bin()
    except EventStoreUnavailable:
        return None
    cmd = [bin_path, "doctor", "event", "rows", "--events", str(events_path)]
    for ty in types or []:
        cmd += ["--type", ty]
    if include_rejected:
        cmd.append("--include-rejected")
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError:
        return None


def read_committed_lines(events_path: Path) -> list[str]:
    """Committed envelope lines, direct SQL, never importing.

    A read with no side effects: retention pruning rides the import the
    rows verb runs, so callers asserting exact store contents (gc, parity)
    use this and stay out of the retention business.
    """
    import sqlite3

    db = store_db_path(events_path)
    if not db.exists():
        return []
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        rows = conn.execute("SELECT line FROM events ORDER BY seq").fetchall()
    finally:
        conn.close()
    return [row[0] for row in rows]


def query_rows(
    events_path: Path,
    *,
    types: Optional[list[str]] = None,
    session_id: Optional[str] = None,
    since_ms: Optional[int] = None,
    limit: Optional[int] = None,
    include_rejected: bool = False,
) -> list[dict[str, Any]]:
    """Committed rows for one journal's store, in commit order, as parsed
    envelopes. A store that does not exist yet is an empty history; a locked
    or corrupt store raises EventStoreUnavailable - unavailable is never
    folded into an empty result."""
    import sqlite3

    db = store_db_path(events_path)
    if not db.exists():
        return []
    where: list[str] = []
    args: list[Any] = []
    if types:
        where.append("type IN (%s)" % ",".join("?" * len(types)))
        args.extend(types)
    if session_id is not None:
        where.append("session_id = ?")
        args.append(session_id)
    if since_ms is not None:
        where.append("ts_ms >= ?")
        args.append(since_ms)
    if not include_rejected:
        where.append("reject_reason IS NULL")
    sql = "SELECT line FROM events"
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY seq"
    if limit is not None:
        sql += f" LIMIT {int(limit)}"
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        rows = conn.execute(sql, args).fetchall()
    except sqlite3.Error as exc:
        raise EventStoreUnavailable(f"event store unreadable at {db}: {exc}") from exc
    finally:
        conn.close()
    out: list[dict[str, Any]] = []
    for (line,) in rows:
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            out.append({"_corrupt": line})
    return out


def gc_ephemeral(
    events_path: Path,
    *,
    ttl_hours: Optional[int] = None,
    dry_run: bool = False,
    now_ms: Optional[int] = None,
) -> dict[str, Any]:
    """Delete expired ephemeral rows from the store; every other class stays.
    With ``dry_run`` nothing is deleted and ``deleted`` reports what the
    horizon WOULD take. The returned shape keeps the historical gc fold
    (scanned/deleted/kept/malformed) so the CLI contract does not drift."""
    import sqlite3
    from fno.events import RETENTION_MINIMUM_TTL_HOURS

    horizon = max(ttl_hours or 0, 0) or RETENTION_MINIMUM_TTL_HOURS
    db = store_db_path(events_path)
    if not db.exists():
        return {
            "scanned": 0,
            "deleted": 0,
            "kept": 0,
            "malformed": 0,
            "ttl_hours": horizon,
        }
    cutoff_ms = (now_ms if now_ms is not None else _now_ms()) - horizon * 3_600_000
    conn = sqlite3.connect(f"file:{db}?mode=rw", uri=True)
    try:
        scanned, malformed = conn.execute(
            "SELECT count(*), coalesce(sum(reject_reason IS NOT NULL), 0) FROM events"
        ).fetchone()
        if dry_run:
            expired = conn.execute(
                "SELECT count(*) FROM events WHERE retention_class = 'ephemeral' AND ts_ms < ?",
                (cutoff_ms,),
            ).fetchone()[0]
            deleted = 0
        else:
            expired = None
            deleted = conn.execute(
                "DELETE FROM events WHERE retention_class = 'ephemeral' AND ts_ms < ?",
                (cutoff_ms,),
            ).rowcount
            conn.commit()
    except sqlite3.Error as exc:
        raise EventStoreUnavailable(f"event store unreadable at {db}: {exc}") from exc
    finally:
        conn.close()
    return {
        "scanned": scanned,
        "deleted": expired if dry_run else deleted,
        "kept": scanned - (expired if dry_run else deleted),
        "malformed": malformed,
        "ttl_hours": horizon,
    }


def _now_ms() -> int:
    import time

    return int(time.time() * 1000)


def emit_envelope(
    envelope: dict[str, Any],
    events_path: Path,
    *,
    requested_id: Optional[str] = None,
    timeout: float = 30,
) -> dict[str, Any]:
    """Commit one canonical ``{ts, type, source, data}`` envelope through the
    native store and return its receipt (``store``, ``event_id``, ``seq``,
    ``retention_class``, ``inserted``). Raises EventStoreUnavailable when the
    store cannot commit."""
    line = json.dumps(envelope, separators=(",", ":"), ensure_ascii=False)
    bin_path = resolve_native_bin()
    cmd = [bin_path, "doctor", "event", "emit-envelope", "--events", str(events_path)]
    if requested_id:
        cmd += ["--id", requested_id]
    try:
        proc = subprocess.run(
            cmd,
            input=line,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        raise EventStoreUnavailable(f"native event store unavailable: {exc}") from exc
    if proc.returncode != 0:
        detail = (proc.stderr or f"exit {proc.returncode}").strip()
        raise EventStoreUnavailable(f"event store refused the write: {detail}")
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise EventStoreUnavailable(
            f"unreadable event store receipt: {proc.stdout[:200]!r}"
        ) from exc
