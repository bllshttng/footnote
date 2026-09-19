"""Terminal-node archive sweep.

Terminal rows (done + superseded) age out of the live board without moving:
the sweep stamps ``archived_at`` on them in the same store, and every default
read filters stamped rows out, so the working graph stays to live work. This
module holds the pure helpers that decide WHAT gets stamped; the stamp itself
is applied inside the sweep's one atomic write (``cmd_archive`` in
graph/cli.py).

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
release rule fixes. Archived ids stay resolvable through the archive-inclusive
reads.
"""
from __future__ import annotations

from datetime import datetime, timezone
from typing import Any, Optional

from fno.graph.statuses import is_terminal_entry

Entry = dict[str, Any]


def _is_terminal(e: Entry) -> bool:
    # The one terminal predicate, homed in statuses: status in TERMINAL_RUNGS,
    # superseded_by, or a non-deferred completed_at. Archive asks "closed for
    # good", NOT "is the work done" (that is node_is_done, the status-only
    # ruling narrowed), so an archived-set fixture stamping only
    # completed_at is terminal here.
    return is_terminal_entry(e)


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


# -- retirement: postmortem receipts -----------------------------------------

#: The retro-triage trailer a landed postmortem carries (source_pr=None is the
#: postmortem shape). A literal match keeps graph/archive free of a retro
#: import; the trailer has one writer (retro.land) and one reader (retro.dedup).
_POSTMORTEM_TRAILER = "retro-triage source_pr=None"


def retire_stale_postmortems(
    entries: list[Entry], now: datetime, max_age_days: int = 30
) -> "tuple[list[Entry], list[Entry]]":
    """Close postmortem receipts that aged out untriaged: ``(entries, retired)``.

    finalize mints one completion-eval node per session and nothing consumed
    them (61 open receipts, 750 near-duplicate pairs). A receipt no human
    touched within ``max_age_days`` closes by rule (status done, a ``retired``
    marker); the next sweep removes it like any terminal row. Queued, claimed,
    deferred, and non-idea rows are never touched. Pure: new dicts.
    """
    cutoff = now.timestamp() - max_age_days * 86400
    patched: list[Entry] = []
    retired: list[Entry] = []
    for e in entries:
        if not isinstance(e, dict) or e.get("status") != "idea":
            patched.append(e)
            continue
        if e.get("queued_at") or e.get("locked_by") or e.get("completed_at"):
            patched.append(e)
            continue
        if _POSTMORTEM_TRAILER not in str(e.get("details") or ""):
            patched.append(e)
            continue
        created = _parse_ts(e.get("created_at"))
        if created is None or created.timestamp() >= cutoff:
            patched.append(e)
            continue
        retired.append({
            **e,
            "status": "done",
            "completed_at": now.isoformat(),
            "retired": "stale-postmortem-receipt",
        })
        patched.append(retired[-1])
    return patched, retired


# -- receipt helpers ----------------------------------------------------------

_ARCHIVE_SKIP_REASONS = (
    "referenced-by-open-node",
    "related-peer-not-archived",
    "too-recent",
    "no-parseable-timestamp",
)


def _archive_bucket_counts(skipped: list) -> dict[str, int]:
    """Tally ``skipped`` by ``_skip`` reason, zero-filled for every known one.

    Zero-fill so a 0 reads as "checked, none held", not as an unmeasured line.
    A reason outside ``_ARCHIVE_SKIP_REASONS`` still lands in the dict, so a
    new ``_skip`` reason shows in the receipt instead of vanishing.
    """
    held = {reason: 0 for reason in _ARCHIVE_SKIP_REASONS}
    for s in skipped:
        held[s["_skip"]] = held.get(s["_skip"], 0) + 1
    return held


def _receipt_reason_order(held: dict[str, int]) -> list[str]:
    extras = set(held) - set(_ARCHIVE_SKIP_REASONS)
    return list(_ARCHIVE_SKIP_REASONS) + sorted(extras)


def _last_sweep_line(now: datetime) -> str:
    """Sweep freshness from the store's newest ``archived_at`` stamp.

    The newest completed_at trails today by the age gate, so it reads as a
    stall; ``archived_at`` is the honest marker. A read failure answers
    unknown, never a clean "none": an instrument that could not read says so.
    """
    from datetime import timedelta

    from fno.graph.store import read_archive_entries

    try:
        entries = read_archive_entries()
    except Exception:  # noqa: BLE001 - the receipt must not crash on a bad archive
        return "unknown (archive unreadable)"
    if not entries:
        return "none on record"
    newest = max(
        (parsed for parsed in
         (_parse_ts(e.get("archived_at")) for e in entries if isinstance(e, dict))
         if parsed is not None),
        default=None,
    )
    if newest is None:
        return "none stamped"
    age = now - newest
    text = (
        "<1h ago" if age < timedelta(hours=1)
        else f"{int(age.total_seconds() // 3600)}h ago" if age < timedelta(hours=48)
        else f"{age.days}d ago"
    )
    return f"{newest.strftime('%Y-%m-%dT%H:%M:%SZ')} ({text})"
