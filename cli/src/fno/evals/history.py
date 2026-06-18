"""Eval run history - append-only JSONL writer.

Each call to ``append_row`` writes exactly ONE newline-terminated JSON line
via a single ``os.write`` with ``O_APPEND | O_CREAT``.  On a local
filesystem, a single ``write()`` syscall under ``O_APPEND`` to a regular
file is atomic: the kernel serialises concurrent appends at the VFS layer
and does not interleave partial writes.  Note: PIPE_BUF governs atomicity
for pipes and FIFOs, not regular files; the guarantee here comes from POSIX
``O_APPEND`` semantics for regular files on a local filesystem.  NFS does
not provide this guarantee.

``iter_rows`` is provided for tooling that needs to read back history.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Iterator


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def append_row(path: Path, row: dict) -> None:
    """Append *row* as a single compact JSON line to *path*.

    The parent directory is created if it does not already exist.  The
    write uses ``O_APPEND | O_CREAT`` so concurrent writers do not
    interleave partial lines on local filesystems (single ``write()``
    under ``O_APPEND`` is atomic for regular files; PIPE_BUF governs
    pipes/FIFOs, not regular files).

    Args:
        path: Destination ``.jsonl`` file (created if absent).
        row:  Serializable dict; must fit in a single line.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(row, separators=(",", ":"), ensure_ascii=False) + "\n"
    encoded = line.encode("utf-8")
    flags = os.O_WRONLY | os.O_APPEND | os.O_CREAT
    fd = os.open(str(path), flags, 0o644)
    try:
        os.write(fd, encoded)
    finally:
        os.close(fd)


def iter_rows(path: Path) -> Iterator[dict]:
    """Yield parsed dicts from *path*, one per non-empty line.

    Silently skips blank lines.  Raises ``json.JSONDecodeError`` on
    malformed lines so callers can decide how to handle corruption.

    Args:
        path: Path to the ``.jsonl`` history file.

    Yields:
        Parsed row dicts in file order.
    """
    if not path.exists():
        return
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            yield json.loads(line)


def iter_rows_tolerant(path: Path) -> Iterator[tuple[int, dict]]:
    """Yield ``(lineno, row)`` from *path*, skipping corrupt lines with a warning.

    Unlike :func:`iter_rows`, this function never raises on malformed input.
    Instead it emits a ``warnings.warn`` naming the line number and continues
    processing remaining lines.  A line is skipped when:
    - it is not valid JSON
    - it parses to something other than a dict (e.g. a bare string or list)

    Blank lines are silently skipped (no warning) and do not affect line
    counting (line numbers match the physical file line numbers, 1-indexed).

    Args:
        path: Path to the ``.jsonl`` history file.

    Yields:
        ``(lineno, row)`` pairs in file order for all parseable rows.
    """
    import warnings

    if not path.exists():
        return
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        warnings.warn(
            f"failed to read evals history file {path}: {exc}",
            stacklevel=2,
        )
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
                f"evals-history.jsonl line {lineno}: skipping non-dict value (type={type(obj).__name__})",
                stacklevel=2,
            )
            continue
        yield lineno, obj
