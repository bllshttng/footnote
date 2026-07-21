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
  - a ``related`` peer of an open node (the edge is symmetric; archiving one
    side would strand the other)
  - the ``source_node_id`` origin of an open node
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
        # isinstance(..., list) before iterating: a legacy/malformed string value
        # would otherwise iterate character-by-character into the guard set.
        blocked_by = e.get("blocked_by")
        if isinstance(blocked_by, list):
            for b in blocked_by:
                if isinstance(b, str):
                    guard.add(b)
        parent = e.get("parent")
        if isinstance(parent, str):
            guard.add(parent)
        supersedes = e.get("supersedes")
        if isinstance(supersedes, list):
            for s in supersedes:
                if isinstance(s, str):
                    guard.add(s)
        # related is symmetric and stored on both endpoints, so archiving one
        # side of a live pair strands the other: the open node names an id the
        # working graph no longer has, and the inverse is beyond set_related's
        # reach. Broken by routine grooming rather than by any explicit edit.
        # An open node's origin, for the same reason: this PR made
        # source_node_id readable (rendered, walked, and counted as capture
        # coverage), so archiving the target turns a live edge into a dangler.
        origin = e.get("source_node_id")
        if isinstance(origin, str):
            guard.add(origin)
        related = e.get("related")
        if isinstance(related, list):
            for r in related:
                if isinstance(r, str):
                    guard.add(r)
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

    # A related pair must move together. `_guard_ids` only protects references
    # held by OPEN nodes, so two terminal peers of different ages would split:
    # the older sweeps while the newer stays behind naming an id the working
    # graph no longer has, and set_related resolves peers against the working
    # graph only, so nothing could repair it. Hold back any candidate whose
    # related peer is staying. Iterated to a fixed point because holding one
    # back can strand the next along a chain; each pass moves at least one
    # entry out, so it terminates.
    while True:
        staying = {
            e.get("id") for e in remaining if isinstance(e.get("id"), str)
        }
        held = [
            e for e in to_archive
            if any(r in staying for r in (e.get("related") or []) if isinstance(r, str))
        ]
        if not held:
            break
        held_ids = {e.get("id") for e in held}
        to_archive = [e for e in to_archive if e.get("id") not in held_ids]
        for e in held:
            remaining.append(e)
            skipped.append({**e, "_skip": "related-peer-not-archived"})

    return to_archive, remaining, skipped


def merge_into_archive(existing: list[Entry], new: list[Entry]) -> list[Entry]:
    """Append ``new`` to ``existing`` archive entries, deduped by id (last wins).

    Dedup makes the crash-window duplicate self-heal: an entry that a crashed
    sweep left in both files is written once here on the next sweep.
    """
    # Track first-seen order without mutating any input dict: last write wins in
    # by_id, and the final list is rebuilt from the recorded order.
    by_id: dict[str, Entry] = {}
    order: list[Any] = []  # node id (str) or the entry itself (id-less)
    for e in [*existing, *new]:
        nid = e.get("id")
        if isinstance(nid, str):
            if nid not in by_id:
                order.append(nid)
            by_id[nid] = e
        else:
            order.append(e)
    return [by_id[x] if isinstance(x, str) else x for x in order]
