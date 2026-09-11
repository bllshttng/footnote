"""Terminal-parent strand family: detect, refuse, and heal stranded children.

`_is_live`/`_live_child_ids` moved here from graph/cli.py so the close
guards, the reconcile self-heal, and the starvation receipts share one
liveness predicate instead of three.
"""
from __future__ import annotations

from typing import Optional

# Deeper than any real epic nesting; mirrors _MAX_ANCESTOR_WALK in advance.py.
_MAX_ANCESTOR_WALK = 64


def _is_live(entry: dict) -> bool:
    """LIVE = not terminal. Terminal is the `recompute_statuses` floor (done
    > superseded > deferred) via completed_at / superseded_by / deferred_at;
    everything else is dispatchable and would strand if its owner died. A
    superseded_by without a verified supersession record still counts dead.
    """
    if entry.get("completed_at") or entry.get("deferred_at"):
        return False
    if not entry.get("superseded_by"):
        return True
    supersession = entry.get("supersession")
    return isinstance(supersession, dict) and not supersession.get("verified_at")


def _live_child_ids(entries: list[dict], owner_id: Optional[str]) -> list[str]:
    """Ids of the owner's live membership children (``parent == owner``).

    Contained children (``contained_in == owner``) are excluded: folding a
    unit releases them routinely, so they are never a reason to refuse. Reads
    liveness, not ``type`` (the epic that prompted this guard was itself typed
    ``feature``).
    """
    if not owner_id:
        return []
    live: list[str] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if e.get("contained_in") == owner_id or e.get("parent") != owner_id:
            continue
        if _is_live(e) and isinstance(e.get("id"), str) and e["id"]:
            live.append(e["id"])
    return live


def _release_contained_children(entries: list[dict], owner_id: Optional[str]) -> list[str]:
    """Un-contain everything shipping inside ``owner_id``; return the ids freed.

    Called wherever a delivery unit permanently dies: remove and supersede. A
    reversible defer keeps its folded delivery unit intact so undefer restores
    the same one-PR scope. A permanently dead unit will never merge, so
    ``_strandable_contained_ids`` (which keys on ``completed_at``) can never heal
    its children, while ``selection_guards`` and ``fno do target init`` keep
    refusing them: unbuildable, uncloseable, invisible to every sweep.

    Un-contained, never closed: a unit dying is not a claim that its children
    shipped.
    """
    if not owner_id:
        return []
    freed: list[str] = []
    for e in entries:
        if isinstance(e, dict) and e.get("contained_in") == owner_id:
            e.pop("contained_in", None)
            nid = e.get("id")
            if isinstance(nid, str) and nid:
                freed.append(nid)
    return freed


def _release_parented_children(entries: list[dict], owner_id: Optional[str]) -> list[str]:
    """Clear ``parent`` on the owner's non-done children; return the ids freed.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    if not owner_id:
        return []
    freed: list[str] = []
    for e in entries:
        if not isinstance(e, dict) or e.get("parent") != owner_id:
            continue
        if e.get("completed_at"):
            continue  # done is truly terminal - keep parent as history
        # Set None (key kept) rather than pop, matching the supported un-adopt
        # path (`update --parent null`) and every other parent writer; readers
        # use .get(), so a present-None reads identically to absent.
        e["parent"] = None
        nid = e.get("id")
        if isinstance(nid, str) and nid:
            freed.append(nid)
    return freed


def _reparent_live_children(
    entries: list[dict], dead_id: Optional[str]
) -> list[tuple[str, Optional[str]]]:
    """Re-parent each live membership child of ``dead_id``; return the pairs.

    The new parent is the dead node's nearest live ancestor, else None (key
    kept - the ``_release_parented_children`` convention). Terminal children
    keep their link as history.
    """
    kids = set(_live_child_ids(entries, dead_id))
    if not kids:
        return []
    by_id = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    # Walk up from dead_id's parent; a cycle back into the dead node stops the
    # walk (never hand the children to the corpse).
    seen: set[str] = set()
    cur = (by_id.get(dead_id) or {}).get("parent")
    target: Optional[str] = None
    steps = 0
    while isinstance(cur, str) and cur and steps < _MAX_ANCESTOR_WALK:
        if cur == dead_id or cur in seen:
            break
        seen.add(cur)
        anc = by_id.get(cur)
        if anc is None:
            break  # missing parent - nothing live to hand the children to
        if _is_live(anc):
            target = cur
            break
        cur = anc.get("parent")
        steps += 1
    moved: list[tuple[str, Optional[str]]] = []
    for e in entries:
        nid = e.get("id") if isinstance(e, dict) else None
        if nid in kids:
            e["parent"] = target
            moved.append((nid, target))
    return moved


def _strandable_orphan_ids(entries: list[dict]) -> set[str]:
    """Live non-contained node ids whose DIRECT parent exists and is terminal.

    Read-only detector, the parent-axis twin of ``_strandable_contained_ids``.
    """
    by_id = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    out: set[str] = set()
    for e in entries:
        if not isinstance(e, dict) or not _is_live(e) or e.get("contained_in"):
            continue
        pid = e.get("parent")
        parent = by_id.get(pid) if isinstance(pid, str) else None
        nid = e.get("id")
        if pid and parent is not None and not _is_live(parent) and isinstance(nid, str) and nid:
            out.add(nid)
    return out


def _sweep_reparent_stranded_orphans(
    entries: list[dict],
) -> list[tuple[str, Optional[str]]]:
    """Re-parent every stranded child on the board; one pass, no fixpoint.

    Every stranded child has a terminal DIRECT parent, and re-parenting never
    creates one (the target is live, or None), so one pass converges.
    """
    stranded = _strandable_orphan_ids(entries)
    if not stranded:
        return []
    by_id = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    parents = {by_id[i].get("parent") for i in stranded}
    moved: list[tuple[str, Optional[str]]] = []
    for pid in sorted(p for p in parents if isinstance(p, str)):
        moved.extend(_reparent_live_children(entries, pid))
    return moved


def _reparent_receipt(pairs: list[tuple[str, Optional[str]]], lead: str = "") -> str:
    """One line per batch. Bare lead is the past tense whose line start groom
    parses (``^re-parented N stranded child``); "Would " previews instead.
    """
    verb = "re-parented" if not lead else "re-parent"
    listed = ", ".join(f"{cid} -> {p or '(none)'}" for cid, p in pairs)
    return f"{lead}{verb} {len(pairs)} stranded child(ren) under terminal parents: {listed}"


def _stranded_next_receipts(receipts: list[tuple[str, str]]) -> list[str]:
    """Capped strand-only advisory lines for a `next` that picked a winner."""
    stranded = [(nid, r) for nid, r in receipts if r == "dead-ancestor"]
    if not stranded:
        return []
    lines = [f"stranded {nid}: {r}" for nid, r in stranded[:10]]
    shown = f" (showing {min(len(stranded), 10)})" if len(stranded) > 10 else ""
    lines.append(
        f"{len(stranded)} node(s) stranded under terminal parents{shown}; "
        "`fno backlog reconcile` re-parents them"
    )
    return lines
