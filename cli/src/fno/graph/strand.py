"""Terminal-parent strand family: detect, refuse, and heal stranded children.

A live node whose DIRECT parent is terminal (done/superseded/deferred) is
stranded: every reader shows it as owned while nothing will ever dispatch it.
`_is_live` and `_live_child_ids` moved here from graph/cli.py so the close
guards, the reconcile self-heal, and the starvation receipts share one
liveness predicate instead of three.
"""
from __future__ import annotations

from typing import Optional

# Deeper than any real epic nesting; mirrors _MAX_ANCESTOR_WALK in advance.py.
_MAX_ANCESTOR_WALK = 64


def _is_live(entry: dict) -> bool:
    """A child is LIVE when it is not terminal: it would strand if its owner died.

    Terminal is the precedence floor in `recompute_statuses` (done > superseded
    > deferred): a node with ``completed_at`` is done, one with
    ``superseded_by`` is superseded, one with ``deferred_at`` is deferred.
    Everything else (idea, ready, blocked, in_review, in_progress) is live and
    dispatchable, so killing its owner without releasing it leaves it
    unbuildable under the dead-ancestor guard.
    """
    if entry.get("completed_at") or entry.get("deferred_at"):
        return False
    if not entry.get("superseded_by"):
        return True
    supersession = entry.get("supersession")
    return isinstance(supersession, dict) and not supersession.get("verified_at")


def _live_child_ids(entries: list[dict], owner_id: Optional[str]) -> list[str]:
    """Ids of the owner's live children that the supersede guard refuses over.

    Membership children only (``parent == owner``), EXCLUDING contained
    children (``contained_in == owner``). The two axes are released differently:
    a contained child is folded delivery work, and superseding the unit
    releases it routinely - that release IS the safety, so it is not a reason
    to refuse. A parent-only child is epic membership; superseding orphans it
    (clearing ``parent``), a structural change the guard exists to consent to.
    This is also why the guard reads liveness, not ``type``: the epic that
    prompted this was itself typed ``feature``.
    """
    if not owner_id:
        return []
    live: list[str] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if e.get("contained_in") == owner_id:
            continue  # folded work - the contained release handles it, not the guard
        if e.get("parent") != owner_id:
            continue
        if not _is_live(e):
            continue
        nid = e.get("id")
        if isinstance(nid, str) and nid:
            live.append(nid)
    return live


def _nearest_live_ancestor(
    entries_by_id: dict, dead_id: str
) -> Optional[str]:
    """First non-terminal ancestor walking up from ``dead_id``, else None."""
    seen: set[str] = set()
    cur = (entries_by_id.get(dead_id) or {}).get("parent")
    steps = 0
    while isinstance(cur, str) and cur and steps < _MAX_ANCESTOR_WALK:
        if cur == dead_id or cur in seen:
            break  # cycle - no trustworthy ancestor
        seen.add(cur)
        anc = entries_by_id.get(cur)
        if anc is None:
            break  # missing parent - nothing live to hand the children to
        if _is_live(anc):
            return cur
        cur = anc.get("parent")
        steps += 1
    return None


def _reparent_live_children(
    entries: list[dict], dead_id: Optional[str]
) -> list[tuple[str, Optional[str]]]:
    """Point each live membership child of ``dead_id`` at a live ancestor.

    Same membership ``_live_child_ids`` refuses over; a terminal child keeps
    its link as history. The new parent is the dead node's nearest live
    ancestor, or None (key kept, set to None - the ``_release_parented_children``
    convention) when the whole ancestor chain is terminal. Returns the
    (child_id, new_parent) pairs it wrote.
    """
    if not dead_id:
        return []
    by_id = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    target = _nearest_live_ancestor(by_id, dead_id)
    moved: list[tuple[str, Optional[str]]] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if e.get("contained_in") == dead_id:
            continue  # folded work - the contained release owns that axis
        if e.get("parent") != dead_id:
            continue
        if not _is_live(e):
            continue
        nid = e.get("id")
        if isinstance(nid, str) and nid:
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
        if not isinstance(e, dict) or not _is_live(e):
            continue
        if e.get("contained_in"):
            continue  # the contained sweep owns that axis
        pid = e.get("parent")
        if not isinstance(pid, str) or not pid:
            continue
        parent = by_id.get(pid)
        nid = e.get("id")
        if parent is not None and not _is_live(parent) and isinstance(nid, str) and nid:
            out.add(nid)
    return out


def _sweep_reparent_stranded_orphans(
    entries: list[dict],
) -> list[tuple[str, Optional[str]]]:
    """Re-parent every stranded child on the board; one pass, no fixpoint.

    Every stranded child has a terminal DIRECT parent, so visiting each
    terminal parent once is enough - and re-parenting never creates a new
    terminal parent (the target is live, or None).
    """
    stranded = _strandable_orphan_ids(entries)
    if not stranded:
        return []
    by_id = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    dead_parents = sorted({by_id[i].get("parent") for i in stranded} - {None})
    moved: list[tuple[str, Optional[str]]] = []
    for pid in dead_parents:
        moved.extend(_reparent_live_children(entries, pid))
    return moved
