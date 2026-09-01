"""Epic-closure rules over the deferred_kind vocabulary.

The measurement behind this module (2026-09-01): six epics report
in_progress forever because every incomplete child is deferred or superseded,
and nothing can raise them. Three rules fix the arithmetic WITHOUT touching a
row: done children never hold an epic open, superseded children never do (the
work moved, it is not outstanding), and a ``wont_do`` deferral never does
(either - it is a decision, not a delay). Every other deferral (unclassified,
contingent, blocked, ...) still holds its epic open and still needs a human
ruling, which is the correct residue: this module surfaces, it never closes.

Read-only by construction: pure functions over reader-tier entries. The
``stuck-epics`` verb is the operator surface.
"""
from __future__ import annotations

from dataclasses import dataclass, field

# A child in one of these statuses is resolved for closure purposes.
_RESOLVED_STATUSES = frozenset({"done", "superseded"})
# Epic statuses that are themselves terminal - a superseded epic is not stuck.
_EPIC_TERMINAL_STATUSES = frozenset({"done", "superseded"})


def holds_epic_open(child: dict) -> bool:
    """True when ``child`` still represents outstanding epic work.

    The two exception rules: ``superseded`` never holds an epic open, and a
    ``wont_do`` deferral never does. Everything else deferred does - most
    importantly an UNCLASSIFIED deferral, because classifying it is the
    operator's judgment call, not this module's.
    """
    status = child.get("status")
    if status == "done":
        return False
    if status == "superseded":
        return False
    if status == "deferred" and child.get("deferred_kind") == "wont_do":
        return False
    return True


@dataclass
class EpicStuck:
    """One epic whose incomplete children are all deferred/superseded."""

    id: str
    status: str | None
    title: str | None
    closable: bool
    holders: list[dict] = field(default_factory=list)
    held_open_by: list[str] = field(default_factory=list)


def stuck_epics(entries: list[dict]) -> list[EpicStuck]:
    """Every epic (a node with children) that nothing can advance.

    Stuck = the epic itself is not done/superseded AND every non-done child
    is deferred or superseded. ``closable`` = stuck, the epic itself is not
    deferred (closing a deferred epic would first need an undefer, which is
    an operator ruling), and no child holds it open. Read-only; never
    mutates and never orders a closure.
    """
    by_id = {
        e.get("id"): e
        for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    out: list[EpicStuck] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        kids = e.get("children") or []
        if not kids:
            continue
        epic_status = e.get("status")
        if epic_status in _EPIC_TERMINAL_STATUSES:
            continue
        holders: list[dict] = []
        all_blocked = True
        for k in kids:
            child = by_id.get(k.get("id"))
            if child is None:
                # A child id pointing nowhere is ignored, same as
                # store._compute_children (no phantom summary).
                continue
            status = child.get("status")
            if status == "done":
                continue
            if status in ("deferred", "superseded"):
                # Nothing can advance here; only the two-rule exceptions
                # (superseded, wont_do) are non-holders.
                if holds_epic_open(child):
                    holders.append(
                        {
                            "id": child.get("id"),
                            "status": status,
                            "deferred_kind": child.get("deferred_kind"),
                        }
                    )
                continue
            # Any other status (in_progress, ready, idea, ...) is work that
            # could still advance: the epic is not stuck.
            all_blocked = False
        if not all_blocked or not kids:
            continue
        out.append(
            EpicStuck(
                id=e["id"],
                status=epic_status,
                title=e.get("title"),
                closable=(epic_status != "deferred" and not holders),
                holders=holders,
                held_open_by=[h["id"] for h in holders],
            )
        )
    return out
