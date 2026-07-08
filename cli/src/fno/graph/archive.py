"""Terminal-node archive sweep + read-through fallback.

58% of the graph is terminal (done + superseded) and every locked read/mutation
pays for the full file. This module moves old terminal entries into a sibling
``graph-archive.json`` (append-only, same shape) under the graph lock, keeping
the working graph to live work. A crash between the two writes duplicates an
entry rather than losing it (archive is written first); read-through resolves
from the working graph first and the next sweep dedupes.

Never archived (an open node still points at them):
  - a blocker in any open node's ``blocked_by``
  - the parent of any open child
  - a ``supersedes`` / ``superseded_by`` target of an open node
"""
from __future__ import annotations

from datetime import datetime, timezone
from typing import Any, Optional

Entry = dict[str, Any]


def _is_done(e: Entry) -> bool:
    return bool(e.get("completed_at"))


def _is_superseded(e: Entry) -> bool:
    return bool(e.get("superseded_by"))


def _is_terminal(e: Entry) -> bool:
    return _is_done(e) or _is_superseded(e)


def _terminal_ts(e: Entry) -> Optional[str]:
    # done -> completed_at; superseded -> updated; fall back to created_at so a
    # timestamped-but-oddly-shaped terminal still has an age.
    return e.get("completed_at") or e.get("updated") or e.get("created_at")


def _parse_ts(ts: Optional[str]) -> Optional[datetime]:
    if not isinstance(ts, str) or not ts.strip():
        return None
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _guard_ids(entries: list[Entry]) -> set[str]:
    """Ids an OPEN node still references - never archive these."""
    guard: set[str] = set()
    for e in entries:
        if _is_terminal(e):
            continue  # only open nodes protect their references
        for b in e.get("blocked_by") or []:
            if isinstance(b, str):
                guard.add(b)
        parent = e.get("parent")
        if isinstance(parent, str):
            guard.add(parent)
        for s in e.get("supersedes") or []:
            if isinstance(s, str):
                guard.add(s)
        sup = e.get("superseded_by")
        if isinstance(sup, str):
            guard.add(sup)
    return guard


def partition_for_archive(
    entries: list[Entry], older_than_days: int, now: datetime
) -> tuple[list[Entry], list[Entry], list[Entry]]:
    """Split entries into (to_archive, remaining, skipped).

    ``skipped`` is the terminal-but-held-back subset (each with a ``_skip``
    reason key added on a shallow copy) so the caller can report why a terminal
    node stayed. ``now`` is injected so the sweep is deterministic in tests.
    """
    cutoff = now.timestamp() - older_than_days * 86400
    guard = _guard_ids(entries)

    to_archive: list[Entry] = []
    remaining: list[Entry] = []
    skipped: list[Entry] = []

    for e in entries:
        if not _is_terminal(e):
            remaining.append(e)
            continue
        nid = e.get("id")
        if isinstance(nid, str) and nid in guard:
            remaining.append(e)
            skipped.append({**e, "_skip": "referenced-by-open-node"})
            continue
        dt = _parse_ts(_terminal_ts(e))
        if dt is None:
            remaining.append(e)
            skipped.append({**e, "_skip": "no-parseable-timestamp"})
            continue
        if dt.timestamp() >= cutoff:
            remaining.append(e)
            skipped.append({**e, "_skip": "too-recent"})
            continue
        to_archive.append(e)

    return to_archive, remaining, skipped


def merge_into_archive(existing: list[Entry], new: list[Entry]) -> list[Entry]:
    """Append ``new`` to ``existing`` archive entries, deduped by id (last wins).

    Dedup makes the crash-window duplicate self-heal: an entry that a crashed
    sweep left in both files is written once here on the next sweep.
    """
    by_id: dict[str, Entry] = {}
    ordered: list[Entry] = []
    for e in [*existing, *new]:
        nid = e.get("id")
        if isinstance(nid, str):
            if nid in by_id:
                by_id[nid].clear()
                by_id[nid].update(e)
                continue
            by_id[nid] = e
        ordered.append(e)
    return ordered
