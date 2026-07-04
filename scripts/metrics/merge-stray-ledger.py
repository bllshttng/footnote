#!/usr/bin/env python3
"""One-off: fold the stray project-local ledger fork into the global ledger.

``register_entry()`` used to dual-write a
project-local ``<repo>/.fno/ledger.json`` alongside the global
``~/.fno/ledger.json`` - the split-brain that forked node-level joins and left
a stray project-local ledger in the repo checkout. The dual-write is removed
(single resolution path); this one-off reconciles the leftover stray:

  1. Fold any rows unique to the stray into the global ledger, idempotent by
     ``session_id`` (a re-run adds nothing - AC1-FR).
  2. Replace the stray FILE with a symlink to the global ledger, so every path
     that still points at ``<repo>/.fno/ledger.json`` (worktree symlinks,
     setup-worktree.sh's link_file, config_cli's activity probe) transparently
     resolves to the single global ledger rather than a dangling path.

Holds the same flock the register path takes (/tmp/abilities-ledger.lock) and
refuses on contention, so a live stop-hook append can never interleave.

Usage:
    python3 scripts/metrics/merge-stray-ledger.py            # dry-run
    python3 scripts/metrics/merge-stray-ledger.py --apply    # write + swap
    python3 scripts/metrics/merge-stray-ledger.py --stray /path/.fno/ledger.json
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import sys
import tempfile
from pathlib import Path

LEDGER_LOCK_PATH = Path("/tmp/abilities-ledger.lock")


def _entries(data: object) -> list[dict]:
    """Extract the row list from either a {'entries': [...]} wrapper or a bare list."""
    if isinstance(data, dict) and isinstance(data.get("entries"), list):
        return data["entries"]
    if isinstance(data, list):
        return data
    return []


def _atomic_write_json(path: Path, data: object) -> None:
    """Temp file in the same dir, fsync, os.replace - a crash leaves the original intact."""
    directory = path.parent
    directory.mkdir(parents=True, exist_ok=True)
    tmp_path: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", dir=str(directory), prefix=f".{path.name}.",
            suffix=".tmp", delete=False, encoding="utf-8",
        ) as tmp:
            tmp_path = tmp.name
            tmp.write(json.dumps(data, indent=2))
            tmp.flush()
            os.fsync(tmp.fileno())
        os.replace(tmp_path, path)
        tmp_path = None
    finally:
        if tmp_path is not None:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass


def _row_key(row: dict) -> tuple[str, str]:
    """Stable dedupe key: the session_id when present, else a fingerprint of the
    row's content. A null/absent session_id must NOT collapse every id-less row
    onto one key - otherwise a single legacy id-less row in global would mask
    every id-less stray row, which would then be lost when the stray is
    replaced by the symlink.
    """
    sid = row.get("session_id")
    if sid:
        return ("sid", str(sid))
    return ("fp", json.dumps(row, sort_keys=True, default=str))


def merge(stray_real: Path, global_path: Path, apply: bool) -> int:
    """Fold stray rows into global (dedupe by session_id, else fingerprint),
    then replace the stray file with a symlink to global."""
    stray_raw = stray_real.read_text(encoding="utf-8")
    stray_rows = _entries(json.loads(stray_raw))
    global_data = json.loads(global_path.read_text(encoding="utf-8"))
    global_rows = _entries(global_data)
    seen = {_row_key(r) for r in global_rows if isinstance(r, dict)}

    to_add: list[dict] = []
    for r in stray_rows:
        if not isinstance(r, dict):
            continue
        key = _row_key(r)
        if key in seen:
            continue
        seen.add(key)  # dedupe within the stray too, not just against global
        to_add.append(r)
    print(f"stray rows: {len(stray_rows)} | already in global: "
          f"{len(stray_rows) - len(to_add)} | to merge: {len(to_add)}")
    for r in to_add:
        print(f"  + {r.get('session_id')}  {str(r.get('title', ''))[:50]!r}")

    if not apply:
        print("\n[dry-run] pass --apply to fold the rows and symlink the stray to global.")
        return 0

    if to_add:
        if isinstance(global_data, dict):
            global_data["entries"] = global_rows + to_add
        else:
            global_data = {"entries": global_rows + to_add}
        _atomic_write_json(global_path, global_data)
        print(f"merged {len(to_add)} row(s) into {global_path}")

    # Back up the stray content before replacing the file (belt-and-suspenders:
    # the rows are already in global, but never unlink the only copy blind).
    backup = stray_real.with_suffix(".json.pre-merge.bak")
    backup.write_text(stray_raw, encoding="utf-8")

    # Replace the stray fork FILE with a symlink to the global ledger so the
    # path stays valid (worktree symlinks / config_cli / setup-worktree.sh)
    # while pointing at the single source of truth.
    stray_real.unlink()
    stray_real.symlink_to(global_path)
    print(f"replaced stray fork with symlink: {stray_real} -> {global_path} "
          f"(backup: {backup})")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--stray", type=Path, default=None,
                    help="Stray project-local ledger. Default: <repo-root>/.fno/ledger.json")
    ap.add_argument("--apply", action="store_true", help="Write (default: dry-run).")
    args = ap.parse_args()

    from fno import paths

    global_path = paths.ledger_json().resolve()
    if args.stray is not None:
        stray_link = args.stray
    else:
        stray_link = paths.resolve_repo_root() / ".fno" / "ledger.json"

    if not stray_link.exists():
        print(f"no stray ledger at {stray_link}; nothing to do.")
        return 0

    stray_real = stray_link.resolve()
    if stray_real == global_path:
        print(f"{stray_link} already resolves to the global ledger; nothing to do.")
        return 0

    lock_fd = os.open(str(LEDGER_LOCK_PATH), os.O_CREAT | os.O_RDWR)
    try:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("ledger lock contended (a live writer holds it); refusing. "
                  "Re-run when idle.", file=sys.stderr)
            return 2
        return merge(stray_real, global_path, args.apply)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


if __name__ == "__main__":
    sys.exit(main())
