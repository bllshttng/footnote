"""Eval run history - append-only JSONL writer.

One newline-terminated JSON line per task-run via a single ``os.write`` under
``O_APPEND | O_CREAT``. A single ``write()`` under ``O_APPEND`` to a regular
file on a local filesystem is atomic: the kernel serialises concurrent appends
at the VFS layer and never interleaves partial writes (POSIX regular-file
``O_APPEND`` semantics; PIPE_BUF governs pipes/FIFOs, not regular files). NFS
does not provide this guarantee.
"""
from __future__ import annotations

import json
import os
from pathlib import Path


def append_row(path: Path, row: dict[str, object]) -> None:
    """Append *row* as a single compact JSON line to *path* (created if absent)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(row, separators=(",", ":"), ensure_ascii=False) + "\n"
    fd = os.open(str(path), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
    try:
        os.write(fd, line.encode("utf-8"))
    finally:
        os.close(fd)


def append_attempt(path: Path, row: dict[str, object]) -> None:
    """Append one ATTEMPT row: refuses a row without its unique identity and
    structured observations - the evidence every verdict classifies from."""
    missing = [k for k in ("attempt_id", "run_id", "obs")
               if not isinstance(row.get(k), (str, dict)) or row.get(k) in ("", None)]
    if missing:
        raise ValueError(f"attempt row is missing required identity/evidence: {missing}")
    append_row(path, row)
