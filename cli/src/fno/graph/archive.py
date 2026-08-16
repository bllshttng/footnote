"""Terminal-node archive sweep + read-through fallback.

58% of the graph is terminal (done + superseded) and every locked read/mutation
pays for the full file. This module moves old terminal entries into a sibling
``graph-archive.json`` (append-only, same shape) under the graph lock, keeping
the working graph to live work. A crash between the two writes duplicates an
entry rather than losing it (archive is written first); read-through resolves
from the working graph first and the next sweep dedupes.

Never archived (an open node still points at them through a HARD edge):
  - a blocker in any open node's ``blocked_by``
  - the parent of any open child
  - a ``supersedes`` / ``superseded_by`` target of an open node

SOFT edges (an open node's ``related`` peer, its ``source_node_id`` origin) do
not hold a terminal node in the working set: the sweep strips the reference on
the open side at apply time (see :func:`release_soft_edges`) and the node
leaves. Hard edges carry dependency or lineage that would break if the target
vanished from the working graph; a soft edge is a navigational convenience, and
keeping finished work pinned forever behind one was the drain failure this
release rule fixes. Read-through fallback keeps the archived id resolvable.
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
    """Ids an OPEN node still references through a HARD edge - never archive these.

    Soft edges (``related``, ``source_node_id``) are deliberately absent: they
    are released on the open side at apply time by :func:`release_soft_edges`
    instead of holding the target. Measured on the machine this was written on,
    203 terminal nodes were pinned purely by soft edges - finished work that
    could never leave the working set because an open node happened to name it
    as related or its origin.
    """
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

    # A related pair of TERMINAL nodes must move together. An OPEN peer no
    # longer holds a candidate: its related edge is soft, stripped at apply by
    # release_soft_edges, so the pair can split along the open/terminal line.
    # Two terminal peers of different ages still would strand - the older
    # sweeps while the newer stays behind naming an id the working graph no
    # longer has, and set_related resolves peers against the working graph
    # only. Hold back any candidate whose terminal peer is staying. Iterated to
    # a fixed point because holding one back can strand the next along a chain;
    # each pass moves at least one entry out, so it terminates.
    while True:
        terminal_staying = {
            e.get("id")
            for e in remaining
            if _is_terminal(e) and isinstance(e.get("id"), str)
        }
        held = [
            e for e in to_archive
            if any(
                r in terminal_staying for r in (e.get("related") or []) if isinstance(r, str)
            )
        ]
        if not held:
            break
        held_ids = {e.get("id") for e in held}
        to_archive = [e for e in to_archive if e.get("id") not in held_ids]
        for e in held:
            remaining.append(e)
            skipped.append({**e, "_skip": "related-peer-not-archived"})

    return to_archive, remaining, skipped


def release_soft_edges(
    remaining: list[Entry], arch_ids: set[str]
) -> tuple[list[Entry], int]:
    """Strip references to ``arch_ids`` from the staying nodes' SOFT edges.

    Removes each archived id from every staying node's ``related`` list and
    nulls a staying node's ``source_node_id`` when it names an archived id.
    HARD edges are untouched (a hard edge to an archived id cannot arise: the
    guard held that target back). Pure: returns new dicts, never mutates the
    input, so the caller applies it under the graph lock in one write.

    Returns ``(patched_remaining, stripped_count)`` where ``stripped_count`` is
    the number of soft-edge references removed (receipt/event material, not a
    gate).
    """
    if not arch_ids:
        return remaining, 0
    patched: list[Entry] = []
    stripped = 0
    for e in remaining:
        out = e
        related = e.get("related")
        if isinstance(related, list):
            kept = [r for r in related if not (isinstance(r, str) and r in arch_ids)]
            if len(kept) != len(related):
                stripped += len(related) - len(kept)
                out = {**out, "related": kept}
        origin = out.get("source_node_id")
        if isinstance(origin, str) and origin in arch_ids:
            stripped += 1
            out = {**out, "source_node_id": None}
        patched.append(out)
    return patched, stripped


def stamp_archived_at(entries: list[Entry], ts: str) -> list[Entry]:
    """Return ``entries`` with ``archived_at`` set to ``ts`` on each (new dicts).

    Pure, never mutates the input. Called once per sweep, right before the
    entries are merged into ``graph-archive.json`` -- nothing recorded WHEN a
    node left the working graph before this, so a "did the last sweep move
    anything" question had no answer on the archive side either.
    """
    return [{**e, "archived_at": ts} for e in entries]


def remint_archive_collisions(
    working_ids: set[str], archive_entries: list[Entry]
) -> tuple[list[Entry], dict[str, str]]:
    """Remint any archive entry whose id collides with a live working-graph id.

    17 such collisions exist on disk (x-f69b): the id generator only checked
    the working graph, so a freed id got reissued while the archive still
    held a different node under it. Reminting the LIVE id would break every
    open reference to it today (blockers, parents, branches, worktrees, open
    PRs); the archived side is passive history, so it moves instead and keeps
    its old id as ``previous_id`` so a stale reference can still resolve
    (``cmd_get``'s archive read-through checks it as a fallback).

    Returns ``(patched_entries, {old_id: new_id})`` for the caller to report.
    A no-op (empty remap) when nothing collides.
    """
    from fno.graph._constants import mint_node_id

    reserved = set(working_ids) | {
        nid for e in archive_entries
        if isinstance(e, dict) and isinstance(nid := e.get("id"), str)
    }
    remap: dict[str, str] = {}
    patched: list[Entry] = []
    for e in archive_entries:
        eid = e.get("id")
        if isinstance(eid, str) and eid in working_ids:
            new_id = mint_node_id(reserved)
            reserved.add(new_id)
            remap[eid] = new_id
            e = {**e, "id": new_id, "previous_id": eid}
        patched.append(e)
    return patched, remap


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
