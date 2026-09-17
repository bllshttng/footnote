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
from pathlib import Path
from typing import Any, Optional


class EventStoreUnavailable(RuntimeError):
    """The native store could not commit the event. No fallback exists."""


def resolve_native_bin() -> str:
    """Resolve the native ``fno`` binary: ``FNO_BIN`` first (the test seam,
    same variable the loopcheck seam uses), else ``PATH``. A missing binary
    is a named failure, never a silent file write."""
    bin_path = os.environ.get("FNO_BIN")
    if bin_path:
        return bin_path
    found = shutil.which("fno")
    if found:
        return found
    raise EventStoreUnavailable(
        "no native fno binary on PATH; the event store has no fallback writer"
    )


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
