"""``fno agents court`` (x-8cee): one read of every crown, its scope, its
holder, and whether the registry and the graph agree.

The read that would have prevented the 2026-08-20 incident. A three-crown
re-scope took five attempts because no single command answered "did the
coronations work" - the grant was recorded as a decision and relayed by mail
while the registry rows kept reading the old scope, and nobody could see the
disagreement.

Agreement is a POSITIVE marker on purpose. A screen that shows no
disagreements because the graph could not be read looks exactly like a
healthy fleet, which is the state that let the incident run five times
instead of once. So ``agree`` is ``True`` only when the graph was actually
read and the scope checked out; an unreadable graph or an external tracker
backend answers ``None`` with a stated reason, and the summary line counts
unknowns separately from disagreements.
"""
from __future__ import annotations

from typing import Any, Optional

from fno.agents.crown import (
    _canonical_project,
    _same_territory,
    crown_reading,
    split_scope,
)


def _by_id() -> Optional[dict[str, dict]]:
    """One graph parse, id -> entry, or ``None`` when it could not be read.

    Matching :func:`fno.agents.crown._stranded_subordinates`: ``None`` is a
    distinct answer from an empty graph, because "unreadable" and "nothing
    here" are not the same fact for a caller deciding whether to trust the
    absence of a disagreement.
    """
    from fno.tracker.metadata import read_entries

    try:
        entries = read_entries("agents.crown")
    except Exception:
        return None
    by_id: dict[str, dict] = {}
    for e in entries:
        node_id = e.get("id") if isinstance(e, dict) else None
        if node_id:
            by_id[node_id] = e
    return by_id


def _agreement(
    level: Optional[int], scope: Optional[str], by_id: Optional[dict[str, dict]]
) -> tuple[Optional[bool], Optional[str]]:
    """Does the graph corroborate this crown? ``(agree, reason)``.

    ``by_id is None`` (graph unreadable / external tracker backend) always
    answers ``(None, reason)`` - never ``True`` and never ``False`` - so an
    unreadable graph can never render as either agreement or disagreement.
    """
    if by_id is None:
        return None, "graph unreadable"
    members = split_scope(scope)
    if level == 2:
        node_id = members[0] if members else None
        entry = by_id.get(node_id) if node_id else None
        if entry is None:
            return False, f"{node_id!r} is not in the graph"
        node_type = entry.get("type")
        if node_type != "epic":
            return False, f"{node_id!r} is a {node_type or 'node'}, not an epic"
        status = entry.get("status")
        if status in ("done", "superseded"):
            return False, f"{node_id!r} status is {status!r} (terminal)"
        return True, None
    # Level 0/1: a portfolio or single-project crown agrees when every member
    # resolves to a configured project - the same check `resolve_crown` makes
    # when the crown was first granted.
    unresolved = [m for m in members if _canonical_project(m) is None]
    if unresolved:
        return False, (
            f"{', '.join(unresolved)} not a configured project"
            if len(unresolved) == 1
            else f"{', '.join(unresolved)} are not configured projects"
        )
    return True, None


def _conflicts(rows: list) -> list[dict[str, Any]]:
    """Territory two live crowned rows hold at once, one entry per scope.

    Reuses :func:`_same_territory` rather than a second equality check, so a
    conflict here and the one-live-crown guard at grant time can never
    disagree about what "same territory" means.
    """
    crowned: list[tuple[Any, dict]] = []
    for row in rows:
        reading = crown_reading(row)
        if reading is not None:
            crowned.append((row, reading))
    seen: list[tuple[str, list[str]]] = []
    for row, reading in crowned:
        scope = reading["scope"]
        for other_scope, holders in seen:
            if _same_territory(other_scope, scope):
                holders.append(row.name)
                break
        else:
            seen.append((scope, [row.name]))
    return [
        {"scope": scope, "holders": holders}
        for scope, holders in seen
        if len(holders) > 1
    ]


def gather_court(rows: Optional[list] = None) -> dict[str, Any]:
    """The whole court: every crown, its verdict, and any territorial conflict.

    ``rows`` overrides the live registry read for callers that already hold
    it (tests); the default reads the registry once here.
    """
    from fno.agents.registry import TERMINAL_STATUSES, load_registry

    if rows is None:
        try:
            rows = load_registry()
        except Exception:
            rows = []
    live_rows = [r for r in rows if r.status not in TERMINAL_STATUSES]

    by_id = _by_id()
    entries: list[dict[str, Any]] = []
    for row in live_rows:
        reading = crown_reading(row)
        if reading is None:
            continue
        agree, reason = _agreement(reading["level"], reading["scope"], by_id)
        entries.append(
            {
                "holder": row.name,
                "level": reading["level"],
                "scope": reading["scope"],
                "grantor": reading["grantor"],
                "status": row.status,
                "agree": agree,
                "reason": reason,
            }
        )

    disagreements = sum(1 for e in entries if e["agree"] is False)
    unknowns = sum(1 for e in entries if e["agree"] is None)
    return {
        "crowns": entries,
        "conflicts": _conflicts(live_rows),
        "graph_readable": by_id is not None,
        "summary": {
            "total": len(entries),
            "disagreements": disagreements,
            "unknowns": unknowns,
        },
    }


def _fmt_row(e: dict[str, Any]) -> str:
    agree = "?" if e["agree"] is None else ("yes" if e["agree"] else "no")
    reason = f"   {e['reason']}" if e["reason"] else ""
    return (
        f"{e['scope']:<16} {e['level']:<5} {e['holder']:<20} "
        f"{e['grantor']:<16} {e['status']:<7} {agree:<4}{reason}"
    )


def render_court(as_json: bool) -> str:
    """The full render: table + conflicts + summary, or its JSON mirror."""
    import json

    court = gather_court()
    if as_json:
        return json.dumps(court, indent=2, sort_keys=True)

    if not court["crowns"]:
        return "court: no live crowns"

    header = f"{'SCOPE':<16} {'LEVEL':<5} {'HOLDER':<20} {'GRANTOR':<16} {'STATUS':<7} AGREE"
    lines = [header] + [_fmt_row(e) for e in court["crowns"]]
    for c in court["conflicts"]:
        holders = ", ".join(c["holders"])
        lines.append(f"\nconflicts: scope {c['scope']!r} held by {len(c['holders'])} live rows ({holders})")
    s = court["summary"]
    lines.append(
        f"\ncourt: {s['total']} crowns, {s['disagreements']} disagreement"
        f"{'s' if s['disagreements'] != 1 else ''}, {s['unknowns']} unknown"
        f"{'s' if s['unknowns'] != 1 else ''}"
    )
    return "\n".join(lines)
