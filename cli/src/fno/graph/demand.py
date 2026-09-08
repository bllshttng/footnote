"""The divergence read over a node's encounters.

The score is encounter weight AGAINST operator priority, because raw count is
the weak reading: a p0 with many encounters says nothing, a p3 with many is the
whole product. Why there is no normalization and no decay, why sybil-by-dispatch
and an operator disagreeing with themselves are DISPLAYED rather than corrected,
and what each column means, are in `docs/backlog-usage.md`.

Nothing here writes. `demand` never touches `rank` and never consults
`_kanban_column` as an input, because the board is the work order and a signal
that reorders it on its own removes the judgement this exists to inform.
`importance_score` is where the read reaches selection, and it is deliberately
the weakest term there.
"""
from __future__ import annotations

# Lower priority number means the operator is looking at it MORE, so it earns
# less divergence. p2 is the default weight for anything unrecognised, matching
# the graph's own default priority.
PRIORITY_WEIGHT = {"p0": 1, "p1": 2, "p2": 3, "p3": 4}
_DEFAULT_WEIGHT = PRIORITY_WEIGHT["p2"]


#: The one shared operator voter key. A literal here and a check everywhere a
#: kind is tested; the constant is what keeps a third module from re-spelling it.
OPERATOR_VOTER_KIND = "operator"


def voter_key(record: dict) -> str:
    """Return the identity that makes an encounter one-per voter."""
    return str(record.get("voter_key") or record.get("session_id") or "")


def recent_encounter(entry: dict, now, within_days: int) -> bool:
    """True when some encounter was recorded inside the window.

    A vote says the node cost somebody time THEN. Unwindowed it would be a
    permanent exemption from the age drain that any agent could switch on with
    no undo, so this reads the ``ts`` the record already carries.
    """
    stamps = (
        _parse_ts(r.get("ts"))
        for r in (entry.get("encounters") or [])
        if isinstance(r, dict)
    )
    return any(s is not None and (now - s).days <= within_days for s in stamps)


def encounter_voters(entry: dict) -> set:
    """Distinct voter keys that recorded an encounter with this node.

    Distinct VOTERS, never rows. The write verb already refuses a second vote per
    voter, so a duplicate row means the record was written some other way;
    counting rows would let that path inflate the signal. The fallback to
    ``session_id`` keeps encounters written before ``voter_key`` was introduced.
    """
    return {
        voter_key(e)
        for e in (entry.get("encounters") or [])
        if isinstance(e, dict) and voter_key(e)
    }


def operator_voters(entry: dict) -> set:
    """The subset of encounter voters that voted under the operator key."""
    return {
        voter_key(e)
        for e in (entry.get("encounters") or [])
        if isinstance(e, dict)
        and e.get("voter_kind") == OPERATOR_VOTER_KIND
        and voter_key(e)
    }


def divergence_score(entry: dict, effective_priority: str, voters: int | None = None) -> int:
    """Encounter weight against operator priority.

    Higher means the operator is looking at it less than the agents are hitting
    it. A node no session was ever sent to, that sessions keep hitting anyway,
    doubles: it is the loudest row available, and it is the one no other
    instrument reports. ``voters`` lets a caller that already built the set
    pass its size rather than walk ``encounters`` a second time.
    """
    weight = PRIORITY_WEIGHT.get(effective_priority, _DEFAULT_WEIGHT)
    if not entry.get("sessions") and not entry.get("pr_number"):
        weight *= 2
    return (len(encounter_voters(entry)) if voters is None else voters) * weight


#: Age is worth at most 90/100 of a point, less than the smallest one vote can
#: be worth (a p0 vote scores 1), so age never buys a vote. It only breaks ties.
_AGE_CAP_DAYS = 90
_AGE_DIVISOR = 100.0


def _parse_ts(value: object):
    """One ISO reader for both clocks here. An unreadable stamp is no signal."""
    from datetime import datetime, timezone

    try:
        stamp = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except (TypeError, ValueError):
        return None
    return stamp if stamp.tzinfo else stamp.replace(tzinfo=timezone.utc)


def importance_score(entry: dict, effective_priority: str, now=None) -> float:
    """Divergence plus age, for the unranked band of the selection key.

    A projection, never stored, and zero for an unvoted node: a score from age
    alone would reorder the backlog on a signal no one recorded. That test runs
    first, so the sort parses no timestamp for the rows that have no vote.
    Difficulty never enters, being a routing axis. Age reads ``touched_at`` and
    falls back to birth; an unparseable stamp is no age signal.
    """
    from datetime import datetime, timezone

    voters = encounter_voters(entry)
    if not voters:
        return 0.0
    divergence = float(divergence_score(entry, effective_priority, len(voters)))
    stamp = _parse_ts(entry.get("touched_at")) or _parse_ts(entry.get("created_at"))
    if stamp is None:
        return divergence
    days = max(0, ((now or datetime.now(timezone.utc)) - stamp).days)
    return divergence + min(days, _AGE_CAP_DAYS) / _AGE_DIVISOR


def _dispatched_count(entry: dict, voters: set) -> int:
    """How many encountering sessions were also dispatched to this node."""
    dispatched = {
        row.get("session_id")
        for row in (entry.get("sessions") or [])
        if isinstance(row, dict) and row.get("session_id")
    }
    return len(voters & dispatched)


def demand_rows(entries: list[dict]) -> list[dict]:
    """One row per node carrying at least one encounter, highest score first.

    Ties break on node id so two runs against one graph render identically.
    """
    from fno.graph._intake import make_effective_priority

    priority_for = make_effective_priority(entries)
    rows: list[dict] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        voters = encounter_voters(entry)
        if not voters:
            continue
        operators = operator_voters(entry)
        rows.append(
            {
                "score": divergence_score(entry, priority_for(entry)),
                "node": entry.get("id"),
                "pri": priority_for(entry),
                "enc": len(voters),
                "agent": len(voters - operators),
                "operator": len(voters & operators),
                "dispatched": _dispatched_count(entry, voters),
                # `status`, not the kanban column. The column is DERIVED at
                # render time and is absent from a stored entry, so reading it
                # here rendered blank on every row of the live graph. Deriving
                # it would also mean consulting the board's column authority,
                # which this read must not do.
                "status": entry.get("status") or "",
                "title": entry.get("title") or "",
            }
        )
    rows.sort(key=lambda row: (-row["score"], row["node"] or ""))
    return rows


def format_rows(rows: list[dict]) -> str:
    """The table, or the one line that says the signal is empty."""
    if not rows:
        return (
            "no encounters recorded yet. An agent files one with "
            "`fno backlog encounter <node> --evidence \"<what it cost>\"`."
        )
    # The enc cell swallows the (Na/No) split at a FIXED width. Appending the
    # split after a bare :>3 shifted dispatched/status/title right on
    # operator-voted rows only, so those columns aligned with nothing. Counts
    # beyond the width overflow it, exactly as a bare :>3 always could.
    enc_width = 12
    header = (
        f"{'score':>5}  {'node':<8} {'pri':<4} {'enc':<{enc_width}} {'dispatched':>10}  "
        f"{'status':<12} title"
    )
    lines = [header]
    for row in rows:
        split = f" ({row['agent']}a/{row['operator']}o)" if row["operator"] else ""
        enc_cell = f"{row['enc']}{split}"
        lines.append(
            f"{row['score']:>5}  {row['node']:<8} {row['pri']:<4} {enc_cell:<{enc_width}} "
            f"{row['dispatched']:>10}  {row['status']:<12} {row['title']}"
        )
    return "\n".join(lines)
