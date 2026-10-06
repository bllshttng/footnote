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
import subprocess
import time
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
    """Resolve the physical store through the native reader without importing."""
    try:
        receipt = subprocess.run(
            [resolve_native_bin(), "doctor", "event", "rows", "--events", str(events_path), "--store-path-only"],
            capture_output=True, text=True, timeout=30, check=True,
        )
        return Path(json.loads(receipt.stdout)["store"])
    except (OSError, subprocess.SubprocessError, ValueError, KeyError, TypeError) as exc:
        raise EventStoreUnavailable(f"event store path unavailable for {events_path}: {exc}") from exc


def native_rows(
    events_path: Path,
    *,
    types: Optional[list[str]] = None,
    include_rejected: bool = False,
    legacy_fallback: bool = False,
    timeout: float = 30,
    projection: Optional[str] = None,
    query: Optional[dict[str, Any]] = None,
) -> Any:
    """One native read pass: import, then committed envelope lines.

    The whole reader contract lives in the binary; this is the transport.
    ``legacy_fallback`` moves the pre-store raw-journal read behind the verb
    too: a journal with no store answers its raw lines. None means the native
    side was unavailable and the caller degrades.
    """
    try:
        bin_path = resolve_native_bin()
    except EventStoreUnavailable:
        if projection:
            raise
        return None
    cmd = [bin_path, "doctor", "event", "rows", "--events", str(events_path)]
    for ty in types or []:
        cmd += ["--type", ty]
    if include_rejected:
        cmd.append("--include-rejected")
    if legacy_fallback:
        cmd.append("--legacy-fallback")
    if projection:
        cmd.append(projection)
    try:
        proc = subprocess.run(cmd, input=json.dumps(query) if query is not None else None,
                              capture_output=True, text=True, timeout=timeout)
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        if projection:
            raise EventStoreUnavailable(f"native event projection unavailable: {exc}") from exc
        return None
    if proc.returncode != 0:
        if projection:
            raise EventStoreUnavailable((proc.stderr or f"native event read exited {proc.returncode}").strip())
        return None
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError:
        return None


def _native_json(cmd_tail: list[str], what: str) -> Any:
    """Run one native event verb and parse its JSON stdout. A failure raises
    with the native diagnostic; it never reads as an empty result."""
    try:
        proc = subprocess.run([resolve_native_bin(), "doctor", "event", *cmd_tail],
                              capture_output=True, text=True, timeout=30)
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        raise EventStoreUnavailable(f"native event {what} unavailable: {exc}") from exc
    if proc.returncode != 0:
        detail = (proc.stderr or f"exit {proc.returncode}").strip()
        raise EventStoreUnavailable(f"native event {what} refused: {detail}")
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise EventStoreUnavailable(f"unreadable native event {what}: {proc.stdout[:200]!r}") from exc


def read_committed_lines(events_path: Path) -> list[str]:
    """Committed envelope lines, never importing.

    A read with no side effects: retention pruning rides the import the
    rows verb runs, so callers asserting exact store contents (gc, parity)
    use this and stay out of the retention business.
    """
    return _native_json(
        ["rows", "--events", str(events_path), "--include-rejected", "--no-import"], "rows"
    )


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
    return read_projection(events_path, "--query-json", {
        "types": types, "session_id": session_id, "since_ms": since_ms,
        "limit": limit, "include_rejected": include_rejected,
    })


def read_projection(events_path: Path, mode: str, query: dict[str, Any]) -> Any:
    """Transport for native read folds. A failed read never becomes empty."""
    result = native_rows(events_path, projection=mode, query=query)
    if not isinstance(result, dict) or result.get("projection") != mode or "rows" not in result:
        raise EventStoreUnavailable(f"native event projection {mode} unavailable for {events_path}")
    return result["rows"]


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
    from fno.events import RETENTION_MINIMUM_TTL_HOURS

    horizon = max(ttl_hours or 0, 0) or RETENTION_MINIMUM_TTL_HOURS
    cutoff_ms = (now_ms if now_ms is not None else _now_ms()) - horizon * 3_600_000
    cmd = ["prune", "--events", str(events_path), "--cutoff-ms", str(cutoff_ms)]
    receipt = _native_json(cmd + (["--dry-run"] if dry_run else []), "prune")
    scanned, expired = receipt["scanned"], receipt["expired"]
    return {
        "scanned": scanned,
        "deleted": expired,
        "kept": scanned - expired,
        "malformed": receipt["malformed"],
        "ttl_hours": horizon,
    }


def _now_ms() -> int:
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
    # The store's 5s busy timeout can expire under fork-heavy contention
    # (concurrent emitters on a loaded runner); a short retry absorbs that
    # tail instead of surfacing it as an unavailable store.
    detail = ""
    for attempt in range(3):
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
        if proc.returncode == 0:
            detail = ""
            break
        detail = (proc.stderr or f"exit {proc.returncode}").strip()
        if "database is locked" not in detail or attempt == 2:
            break
        time.sleep(0.25 * (attempt + 1))
    if detail:
        if proc.returncode == 3:
            # Exit 3 is a judged refusal (the door marks it): the diagnostic
            # is the contract, and it is a validation error, not a store
            # fault. Deferred import: fno.events imports this module at
            # package load, so the error type resolves at call time only.
            from fno.events import ValidationError

            raise ValidationError(detail.removeprefix("error: "))
        raise EventStoreUnavailable(f"event store refused the write: {detail}")
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise EventStoreUnavailable(
            f"unreadable event store receipt: {proc.stdout[:200]!r}"
        ) from exc
