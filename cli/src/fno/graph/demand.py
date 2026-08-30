"""The divergence read over a node's encounters.

Raw encounter count is the weak reading. A p0 with many encounters tells the
operator nothing, because they already ranked it. A p3 or a never-dispatched
node with many encounters is the entire product: it shows the shape of what the
operator is not looking at. So the score is encounter weight AGAINST operator
priority, and the table sorts by it.

The goal is an INTERRUPT, not a ranking. The bar is "surprising and true", far
below accuracy, so there is no normalization and no decay: plain integer
arithmetic a reader can verify by hand. Sybil-by-dispatch and hot-path bias are
DISPLAY concerns here, not corrections. `dispatched` renders the dispatch
context beside the number rather than subtracting it out, because withholding
the signal is worse than showing it with its context.

An operator vote and an operator priority are two expressions from one person,
so a p3 node the operator voted on and never ranked scores 4 on the strength of
the operator disagreeing with themselves. That is not corrected. It is
DISPLAYED: `enc 1 (0a/1o)` tells the reader which kind of disagreement they are
looking at, and "four agents hit this and I never ranked it" is a different fact
from "I hit this once and never ranked it". Both are worth knowing. Withholding
the row is worse than showing it with its provenance.

Nothing in this module writes. `demand` never touches `rank` and never consults
`_kanban_column` as an input, because the board is the work order and a signal
that reorders it on its own removes the judgement this feature exists to inform.
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


def divergence_score(entry: dict, effective_priority: str) -> int:
    """Encounter weight against operator priority.

    Higher means the operator is looking at it less than the agents are hitting
    it. A node no session was ever sent to, that sessions keep hitting anyway,
    doubles: it is the loudest row available, and it is the one no other
    instrument reports.
    """
    weight = PRIORITY_WEIGHT.get(effective_priority, _DEFAULT_WEIGHT)
    if not entry.get("sessions") and not entry.get("pr_number"):
        weight *= 2
    return len(encounter_voters(entry)) * weight


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
