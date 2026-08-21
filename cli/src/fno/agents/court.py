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
    _graph_index,
    _territory_key,
    crown_reading,
    split_scope,
)
from fno.plan._status import TERMINAL_STATUSES as PLAN_TERMINAL_STATUSES


def _by_id() -> Optional[dict[str, dict]]:
    """One graph parse, id -> entry, or ``None`` when it could not be read.

    Matching :func:`fno.agents.crown._stranded_subordinates`: ``None`` is a
    distinct answer from an empty graph, because "unreadable" and "nothing
    here" are not the same fact for a caller deciding whether to trust the
    absence of a disagreement.
    """
    return _graph_index()


def _agreement(
    level: Optional[int], scope: Optional[str], by_id: Optional[dict[str, dict]]
) -> tuple[Optional[bool], Optional[str]]:
    """Does the graph corroborate this crown? ``(agree, reason)``.

    A crown the graph cannot adjudicate answers ``(None, reason)`` - never
    ``True`` and never ``False`` - so an unreadable graph can never render as
    either agreement or disagreement. That bail is scoped to the rung that
    actually needs the graph: the epic rung reads it, while the project and
    portfolio rungs resolve entirely from config, so an external tracker
    backend must not blank out an answer that is fully determinate.
    """
    members = split_scope(scope)
    # A row carrying a level but no scope is half a crown: crown_label renders
    # it "L1 ?", so it reaches this view. It rules no territory, so it can
    # never agree - and it must not fall through to the emptiness-blind
    # checks below, where an empty member list reads as "nothing unresolved".
    if not members:
        return False, "the row carries a crown level but no scope (half a crown)"
    if level == 2:
        if by_id is None:
            return None, "graph unreadable"
        node_id = members[0]
        entry = by_id.get(node_id)
        if entry is None:
            return False, f"{node_id!r} is not in the graph"
        node_type = entry.get("type")
        if node_type != "epic":
            return False, f"{node_id!r} is a {node_type or 'node'}, not an epic"
        status = entry.get("status")
        if status in PLAN_TERMINAL_STATUSES:
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
    # Joined on crown_scope, NOT on a full crown_reading. crown_reading gates on
    # crown_label, which is None whenever crown_level is None, so a corrupted
    # scope-without-level row would be skipped here while gather_court above
    # deliberately surfaces it. The two halves of one read would then disagree:
    # the row shows as a disagreement and its scope shows as unconflicted, so a
    # caller gating on `conflicts` reads "no territorial overlap" while two live
    # rows claim the same territory. A scope is a claim on territory whether or
    # not a level was recorded beside it.
    claims: list[tuple[Any, str]] = []
    for row in rows:
        scope = getattr(row, "crown_scope", None)
        if isinstance(scope, str) and scope.strip():
            claims.append((row, scope))
    seen: dict[frozenset[str], tuple[str, list[str]]] = {}
    for row, scope in claims:
        key = _territory_key(scope)
        if not key:
            continue
        existing = seen.get(key)
        if existing is None:
            seen[key] = (scope, [row.name])
        else:
            existing[1].append(row.name)
    return [
        {"scope": scope, "holders": holders}
        for scope, holders in seen.values()
        if len(holders) > 1
    ]


def gather_court(rows: Optional[list] = None) -> dict[str, Any]:
    """The whole court: every crown, its verdict, and any territorial conflict.

    ``rows`` overrides the live registry read for callers that already hold
    it (tests); the default reads the registry once here.

    An unreadable REGISTRY nulls ``crowns`` and every summary count rather
    than reporting an empty court. Degrading to ``[]`` here would reproduce,
    one layer below the agreement verdict, exactly the absence-lie this
    module exists to prevent: a caller gating on ``summary.disagreements ==
    0`` would read a healthy fleet from a read that saw nothing at all. A
    null fails that gate instead of passing it.
    """
    from fno.agents.registry import TERMINAL_STATUSES, load_registry

    if rows is None:
        try:
            rows = load_registry()
        except Exception as exc:
            return {
                "crowns": None,
                "conflicts": None,
                "registry_readable": False,
                "graph_readable": None,
                "summary": {
                    "total": None,
                    "disagreements": None,
                    "unknowns": None,
                    "reason": f"registry unreadable: {exc}",
                },
            }
    live_rows = [r for r in rows if r.status not in TERMINAL_STATUSES]

    by_id = _by_id()
    entries: list[dict[str, Any]] = []
    for row in live_rows:
        reading = crown_reading(row)
        if reading is None:
            # crown_reading gates on crown_label, which registry.py returns
            # None whenever crown_level is None - REGARDLESS of crown_scope.
            # A corrupted row carrying a scope with no level would otherwise
            # vanish here: not counted, not flagged unknown, not flagged
            # disagreeing. That absence-lie is the exact defect this module
            # exists to prevent, so surface the anomaly instead of skipping a
            # row that plainly has SOME crown data.
            if getattr(row, "crown_scope", None):
                entries.append(
                    {
                        "holder": row.name,
                        "level": row.crown_level,
                        "scope": row.crown_scope,
                        "grantor": getattr(row, "crown_grantor", None) or "human",
                        "status": row.status,
                        "agree": False,
                        "reason": "half a crown: scope is set but level is missing",
                    }
                )
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
        "registry_readable": True,
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
    # Every cell goes through str() before padding: a half-crown row carries a
    # null scope (crown_label still renders it "L1 ?"), and formatting None
    # with a width raises TypeError - which would crash the one read meant to
    # SURFACE that corruption, on exactly the input it exists to show.
    return (
        f"{str(e['scope']):<16} {str(e['level']):<5} {str(e['holder']):<20} "
        f"{str(e['grantor']):<16} {str(e['status']):<7} {agree:<4}{reason}"
    )


def render_court(as_json: bool) -> str:
    """The full render: table + conflicts + summary, or its JSON mirror."""
    import json

    court = gather_court()
    if as_json:
        return json.dumps(court, indent=2, sort_keys=True)

    if court["crowns"] is None:
        return f"court: CANNOT READ - {court['summary']['reason']}. This is not an empty court; nothing was checked."
    if not court["crowns"]:
        return "court: no live crowns"

    header = f"{'SCOPE':<16} {'LEVEL':<5} {'HOLDER':<20} {'GRANTOR':<16} {'STATUS':<7} AGREE"
    lines = [header] + [_fmt_row(e) for e in court["crowns"]]
    for c in court["conflicts"]:
        holders = ", ".join(c["holders"])
        lines.append(f"\nconflicts: scope {c['scope']!r} held by {len(c['holders'])} live rows ({holders})")
    s = court["summary"]
    lines.append(
        f"\ncourt: {s['total']} crown{'s' if s['total'] != 1 else ''}, "
        f"{s['disagreements']} disagreement"
        f"{'s' if s['disagreements'] != 1 else ''}, {s['unknowns']} unknown"
        f"{'s' if s['unknowns'] != 1 else ''}"
    )
    return "\n".join(lines)
