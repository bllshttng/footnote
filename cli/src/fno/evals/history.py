"""Eval run history - append-only JSONL writer.

One newline-terminated JSON line per task-run via a single ``os.write`` under
``O_APPEND | O_CREAT``. A single ``write()`` under ``O_APPEND`` to a regular
file on a local filesystem is atomic: the kernel serialises concurrent appends
at the VFS layer and never interleaves partial writes (POSIX regular-file
``O_APPEND`` semantics; PIPE_BUF governs pipes/FIFOs, not regular files). NFS
does not provide this guarantee.

``iter_rows_tolerant`` reads back history skipping corrupt lines with a warning,
so an interrupted sweep's partial final line never crashes the reader (AC5-FR).
"""
from __future__ import annotations

import json
import os
import warnings
from pathlib import Path
from typing import Iterator


def append_row(path: Path, row: dict[str, object]) -> None:
    """Append *row* as a single compact JSON line to *path* (created if absent)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(row, separators=(",", ":"), ensure_ascii=False) + "\n"
    fd = os.open(str(path), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
    try:
        os.write(fd, line.encode("utf-8"))
    finally:
        os.close(fd)


def iter_rows(path: Path) -> Iterator[dict[str, object]]:
    """Yield parsed dicts from *path*, one per non-empty line.

    Raises ``json.JSONDecodeError`` on malformed lines; use
    :func:`iter_rows_tolerant` for corruption-safe reads.
    """
    if not path.exists():
        return
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            yield json.loads(line)


def iter_rows_tolerant(path: Path) -> Iterator[tuple[int, dict[str, object]]]:
    """Yield ``(lineno, row)`` from *path*, skipping corrupt lines with a warning.

    Never raises on malformed input: a non-JSON line, a partial final line from
    an interrupted append, or a non-dict value is skipped with a
    ``warnings.warn`` naming the line number (AC5-FR).
    """
    if not path.exists():
        return
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        warnings.warn(f"failed to read evals history {path}: {exc}", stacklevel=2)
        return
    for lineno, raw_line in enumerate(lines, start=1):
        stripped = raw_line.strip()
        if not stripped:
            continue
        try:
            obj = json.loads(stripped)
        except json.JSONDecodeError as exc:
            warnings.warn(
                f"evals-history.jsonl line {lineno}: skipping malformed JSON: {exc}",
                stacklevel=2,
            )
            continue
        if not isinstance(obj, dict):
            warnings.warn(
                f"evals-history.jsonl line {lineno}: skipping non-dict value "
                f"(type={type(obj).__name__})",
                stacklevel=2,
            )
            continue
        yield lineno, obj
