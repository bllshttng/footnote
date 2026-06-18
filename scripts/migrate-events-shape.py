#!/usr/bin/env python3
"""One-shot migration: legacy events.jsonl shape -> canonical envelope.

Rewrites every row of every events.jsonl in the repo from
``{timestamp, source, type, data}`` (legacy) to
``{ts, type, source, data}`` (canonical, per docs/architecture/events-schema.yaml).

Walks:
  - <root>/.fno/events.jsonl
  - <root>/cli/.fno/events.jsonl
  - <root>/.fno/artifacts/events.jsonl
  - <root>/.claude/worktrees/*/.fno/events.jsonl

Properties:
  - Idempotent: re-running on canonical-only files is a no-op (byte-for-byte
    equal output, no .bak written).
  - Stream processing: line-at-a-time iteration; safe for million-row files.
  - Corrupt rows: preserved verbatim, logged to ``<file>.corrupt``,
    migration continues processing subsequent rows.
  - Lock-shared: acquires the same mkdir-based mutex
    (``<file>.lock.d``) that scripts/lib/set-gate.sh uses, so a live target
    session and a migration run cross-serialize. ``MIGRATE_LOCK_TIMEOUT_SECONDS``
    overrides the 30s default (used by tests).

Exit codes:
  0  success
  2  lock timeout (refused to race a live session)

Usage:
  python3 scripts/migrate-events-shape.py [--root PATH] [--dry-run]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

DEFAULT_TIMEOUT = int(os.environ.get("MIGRATE_LOCK_TIMEOUT_SECONDS", "30"))


def _is_legacy(row: dict) -> bool:
    return "timestamp" in row and "ts" not in row


def _migrate_row(row: dict) -> dict:
    return {
        "ts": row["timestamp"],
        "type": row.get("type", ""),
        "source": row.get("source", ""),
        "data": row.get("data", {}),
    }


def _acquire_lock(lock_dir: Path, timeout: int) -> bool:
    """mkdir-based mutex; returns True on acquire, False on timeout."""
    lock_dir.parent.mkdir(parents=True, exist_ok=True)
    deadline = time.monotonic() + timeout
    while True:
        try:
            lock_dir.mkdir()
            return True
        except FileExistsError:
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.1)


def _release_lock(lock_dir: Path) -> None:
    try:
        lock_dir.rmdir()
    except OSError:
        # Best-effort; another process may have cleaned up first.
        pass


def _migrate_file(path: Path, dry_run: bool) -> tuple[int, int, int]:
    """Returns (migrated, skipped, corrupt). Acquires the file's mkdir lock."""
    if not path.is_file():
        return (0, 0, 0)

    lock_dir = path.parent / (path.name + ".lock.d")
    if not _acquire_lock(lock_dir, DEFAULT_TIMEOUT):
        print(
            f"{path}: session active, refused to race (lock timeout)",
            file=sys.stderr,
        )
        raise SystemExit(2)

    try:
        return _do_migrate(path, dry_run)
    finally:
        _release_lock(lock_dir)


def _do_migrate(path: Path, dry_run: bool) -> tuple[int, int, int]:
    migrated = skipped = corrupt = 0
    bak = path.with_suffix(path.suffix + ".bak")
    corrupt_log = path.with_suffix(path.suffix + ".corrupt")
    tmp = path.with_suffix(path.suffix + ".tmp")

    with path.open("r", encoding="utf-8") as fin, tmp.open("w", encoding="utf-8") as fout:
        for lineno, line in enumerate(fin, 1):
            stripped = line.strip()
            if not stripped:
                fout.write(line)
                continue
            try:
                row = json.loads(stripped)
            except json.JSONDecodeError:
                fout.write(line)
                with corrupt_log.open("a", encoding="utf-8") as clog:
                    clog.write(f"line {lineno}: {line}")
                corrupt += 1
                continue

            if _is_legacy(row):
                new_row = _migrate_row(row)
                fout.write(json.dumps(new_row) + "\n")
                migrated += 1
            else:
                fout.write(line)
                skipped += 1

    if dry_run:
        tmp.unlink()
        return (migrated, skipped, corrupt)

    if migrated == 0:
        # No legacy rows. Output bytes equal input bytes (we wrote each line
        # verbatim) but skip the rename to keep mtime stable - lets idempotent
        # second runs be detectable as no-ops.
        tmp.unlink()
        return (0, skipped, corrupt)

    # Atomic rename. Preserve a .bak of the pre-migration file on first
    # migration only; subsequent runs (which would be no-ops anyway) do not
    # overwrite the bak.
    if not bak.exists():
        # Copy the original to .bak before replacing; using replace would
        # lose the original.
        bak.write_bytes(path.read_bytes())
    tmp.replace(path)

    msg = f"{path}: {migrated} migrated, {skipped} skipped"
    if corrupt:
        msg += f", {corrupt} corrupt (preserved verbatim, see {corrupt_log})"
    print(msg)
    return (migrated, skipped, corrupt)


def _walk(root: Path) -> list[Path]:
    candidates: list[Path] = []
    for relpath in (
        ".fno/events.jsonl",
        "cli/.fno/events.jsonl",
        ".fno/artifacts/events.jsonl",
    ):
        p = root / relpath
        if p.is_file():
            candidates.append(p)

    worktrees_root = root / ".claude/worktrees"
    if worktrees_root.is_dir():
        for wt in worktrees_root.iterdir():
            try:
                p = wt / ".fno/events.jsonl"
                if p.is_file():
                    candidates.append(p)
            except OSError:
                # Worktree removed or unreadable mid-walk; skip silently.
                continue

    return candidates


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=os.getcwd(), help="Repo root to walk (default: cwd)")
    parser.add_argument("--dry-run", action="store_true", help="Show what would change without writing")
    args = parser.parse_args(argv)

    root = Path(args.root).resolve()
    files = _walk(root)
    if not files:
        print(f"no events.jsonl files found under {root}", file=sys.stderr)
        return 0

    total_migrated = total_skipped = total_corrupt = 0
    for f in files:
        m, s, c = _migrate_file(f, dry_run=args.dry_run)
        total_migrated += m
        total_skipped += s
        total_corrupt += c

    suffix = " (dry-run, no files modified)" if args.dry_run else ""
    print(
        f"total: {total_migrated} migrated, {total_skipped} skipped, "
        f"{total_corrupt} corrupt across {len(files)} files{suffix}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
