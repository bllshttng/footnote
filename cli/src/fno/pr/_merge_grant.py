"""Typed durable-grant resolver: one answer for status, merge, and the watcher.

A dispatch's merge authority used to live only in the worker's own
``.fno/target-state.md`` manifest, which dies with the worker's session. The
spawner now records the verdict on the worker's append-only ``phase: do`` graph
row (``merge_grant``), so authority outlives the worker that earned it. This
module is the ONE reader of that record: ``fno do pr status`` projects it, the
merge gate can be driven by it, and the PR watcher acts on it - three callers,
never three precedence orders.

The contract, fail-closed in every arm:

* Absence never grants. No graph-linked node, or no receipt on any do row,
  reads ``absent`` - a pre-grant-era node is unrecorded, not approved.
* Malformed never grants. A receipt the writer could not have minted (unknown
  keys, a non-bool ``approved``, an unparseable timestamp) reads ``unknown``.
* Ambiguity never grants. Two nodes linked to one PR, or newest receipts that
  disagree at the same instant, read ``unknown``.
* Newest explicit receipt wins. A newer ``approved=false`` (a ``--no-merge``
  re-dispatch) outranks an older grant; a newer grant outranks an older
  refusal. Ordering is by the receipt's own ``recorded_at``, never by row
  position.
* Only a positively not-live holder transfers execution. The claim states
  ``free``/``stale`` are unheld; ``live``/``suspect`` hold the merge (a worker
  is still on the node); ``corrupted`` reads ``unknown``. This reuses
  :func:`fno.claims.core.claim_status` - the same classification every other
  consumer reads - never a second liveness probe.
* Live config still decides. A receipt is a record of what the spawner
  resolved at dispatch time; ``auto_merge.enabled`` and ``grant`` are re-read
  live, so an operator flipping the switch off revokes every stored receipt
  without touching the graph. An unreadable config reads ``unknown`` here -
  for an EXECUTION decision, an unreadable config is not a verdict.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping, Optional

# The verdict vocabulary. Callers branch on these symbols, never on the
# reason prose: ``granted`` is the only state that authorizes a merge call.
GRANTED = "granted"
REFUSED = "refused"
HELD = "held"
ABSENT = "absent"
UNKNOWN = "unknown"

# Claim states under which nobody holds the node (claims/cli.py's own
# _UNHELD_STATES convention): restated here as the resolver's liveness arm.
_UNHELD_CLAIM_STATES = frozenset({"free", "stale"})

_GRANT_KEYS = {"approved", "source", "recorded_by", "recorded_at"}


@dataclass(frozen=True)
class GrantVerdict:
    """The resolver's typed answer for one node+PR.

    ``state`` is one of the module constants; ``reason`` is human text for a
    receipt line. ``grant`` carries the winning receipt verbatim (only when a
    receipt was selected); ``node_id`` and ``claim_state`` name the scope and
    the liveness reading the verdict was computed from.
    """

    state: str
    reason: str
    node_id: Optional[str] = None
    grant: Optional[Mapping[str, Any]] = None
    claim_state: Optional[str] = None

    @property
    def merge_eligible(self) -> bool:
        """True only for ``granted`` - the one state a merge call may act on."""
        return self.state == GRANTED

    def as_projection(self) -> dict:
        """The receipt-shape ``fno do pr status`` embeds in its payload."""
        return {
            "state": self.state,
            "reason": self.reason,
            "node_id": self.node_id,
            "claim_state": self.claim_state,
        }


def _malformed_grant_reason(grant: Mapping[str, Any]) -> Optional[str]:
    """Why this receipt is unreadable, or None when it is well-formed.

    Mirrors the write-time validation in ``append_session_record``: anything
    the writer would have refused arriving on a row is external tampering or
    an old shape, and both read UNKNOWN here rather than being mined for a
    partial answer.
    """
    if not isinstance(grant, Mapping):
        return "merge_grant is not a mapping"
    unknown = set(grant) - _GRANT_KEYS
    if unknown:
        return f"merge_grant carries unknown keys: {sorted(unknown)}"
    missing = _GRANT_KEYS - set(grant)
    if missing:
        return f"merge_grant is missing keys: {sorted(missing)}"
    if not isinstance(grant.get("approved"), bool):
        return "merge_grant.approved is not a boolean"
    for key in ("source", "recorded_by", "recorded_at"):
        value = grant.get(key)
        if not isinstance(value, str) or not value.strip():
            return f"merge_grant.{key} is not a non-empty string"
    # The writer mints exactly the canonical "...Z" shape, and newest-wins
    # orders receipts by RAW string comparison: a non-canonical but valid UTC
    # spelling ("+00:00") would read well-formed here yet sort arbitrarily
    # against canonical rows, letting a same-instant disagreement slip the
    # tiebreak below. Only the exact canonical form is a receipt.
    if _utc_stamp(grant["recorded_at"]) != grant["recorded_at"].strip():
        return (
            f"merge_grant.recorded_at is not the canonical UTC stamp the "
            f"writer mints: {grant['recorded_at']!r}"
        )
    return None


def _utc_stamp(value: str) -> Optional[str]:
    """Normalize an ISO-8601 UTC stamp to the canonical ``...Z`` form, or None."""
    from datetime import datetime, timedelta, timezone

    try:
        parsed = datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None or parsed.utcoffset() != timedelta(0):
        return None
    return parsed.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def resolve_durable_grant(
    pr_number: int,
    repo: str,
    *,
    graph_path: Optional[Any] = None,
) -> GrantVerdict:
    """Resolve the durable merge verdict for the node this PR delivers.

    Pure decision work over local reads (graph, claims, config): no gh call,
    so status and the watcher can afford it per tick. Never raises - every
    failure mode is a verdict, because a caller that catches instead would
    have to invent its own fail-closed answer, and the whole point of one
    resolver is that there is exactly one.
    """
    from fno.graph.store import GraphUnreadableError, find_nodes_for_pr, read_graph_strict

    from fno.paths import graph_json

    path = graph_path if graph_path is not None else graph_json()
    try:
        entries = read_graph_strict(path)
    except GraphUnreadableError as exc:
        return GrantVerdict(UNKNOWN, f"graph unreadable, refusing to resolve a grant: {exc}")
    except Exception as exc:  # noqa: BLE001 - a resolver never raises
        return GrantVerdict(UNKNOWN, f"graph read failed: {type(exc).__name__}: {exc}")

    from fno.pr._coverage_gate import _repo_slug

    matches = find_nodes_for_pr(path, pr_number, repo=_repo_slug(repo))
    if not matches:
        return GrantVerdict(ABSENT, "no graph-linked node carries this PR")
    if len(matches) > 1:
        return GrantVerdict(
            UNKNOWN,
            f"{len(matches)} nodes link to this PR ({', '.join(sorted(matches))}); "
            "an ambiguous scope never grants",
        )
    node_id = matches[0]

    node = next((e for e in entries if e.get("id") == node_id), None)
    if node is None:
        # find_nodes_for_pr saw it; this pass did not. The graph moved between
        # the two reads - not decidable, and the watcher will see it settle.
        return GrantVerdict(UNKNOWN, "node vanished between graph reads")

    receipts = []
    for row in node.get("sessions") or []:
        if not isinstance(row, dict) or row.get("phase") != "do":
            continue
        grant = row.get("merge_grant")
        if grant is None:
            # No key = the honest pre-field row: no grant was resolved at
            # stamp time. It is evidence of nothing and is skipped, not mined.
            continue
        bad = _malformed_grant_reason(grant)
        if bad:
            return GrantVerdict(UNKNOWN, f"{bad} (node {node_id})")
        receipts.append(dict(grant))
    if not receipts:
        return GrantVerdict(ABSENT, "no do row on the node records a merge grant")

    newest_stamp = max(r["recorded_at"] for r in receipts)
    newest = [r for r in receipts if r["recorded_at"] == newest_stamp]
    approved_flags = {r["approved"] for r in newest}
    if len(approved_flags) > 1:
        return GrantVerdict(
            UNKNOWN,
            "newest durable grants disagree at "
            f"{newest_stamp} (approved={[r['approved'] for r in newest]}); "
            "an ambiguous verdict never grants",
        )
    receipt = newest[0]
    if not receipt["approved"]:
        return GrantVerdict(
            REFUSED,
            "newest durable grant records approved=false "
            f"(source: {receipt['source']}, recorded {newest_stamp})",
            node_id=node_id,
            grant=receipt,
        )

    from fno.claims.core import claim_status

    status = claim_status(f"node:{node_id}")
    claim_state = str(status.get("state"))
    if claim_state == "corrupted":
        return GrantVerdict(
            UNKNOWN,
            "node claim unreadable: " + str(status.get("error") or "corrupted"),
            node_id=node_id,
            grant=receipt,
            claim_state=claim_state,
        )
    if claim_state not in _UNHELD_CLAIM_STATES:
        return GrantVerdict(
            HELD,
            f"node claim is {claim_state} (holder: "
            f"{status.get('holder') or 'unknown'}); only a positively not-live "
            "holder transfers execution",
            node_id=node_id,
            grant=receipt,
            claim_state=claim_state,
        )

    try:
        from fno.config import load_settings_for_repo
        from pathlib import Path

        settings = load_settings_for_repo(Path(repo))
        am = settings.auto_merge
    except Exception as exc:  # noqa: BLE001 - an execution decision fails closed
        return GrantVerdict(
            UNKNOWN,
            f"live config unreadable ({type(exc).__name__}: {exc}); refusing to "
            "execute on an unverifiable standing grant",
            node_id=node_id,
            grant=receipt,
            claim_state=claim_state,
        )
    if not am.enabled:
        return GrantVerdict(
            HELD,
            "receipt recorded an approved dispatch but live config resolves "
            "auto_merge.enabled=false; the standing switch revokes stored "
            "receipts",
            node_id=node_id,
            grant=receipt,
            claim_state=claim_state,
        )
    if str(am.grant or "none") != "dispatch":
        return GrantVerdict(
            HELD,
            f"live config resolves auto_merge.grant={am.grant!r}, not dispatch; "
            "the recorded receipt does not widen it",
            node_id=node_id,
            grant=receipt,
            claim_state=claim_state,
        )

    # The automerge floor: a stored receipt widens WHO may execute, never WHAT
    # review the merge needs. A repo resolved below the self_review rung holds
    # even with a perfect receipt, and an unreadable posture verdict is not a
    # pass (an execution decision fails closed, same as the config arm above).
    from fno.config import resolve_review_posture
    from fno.review_capability import automerge_floor_refusal

    try:
        resolved_posture = resolve_review_posture(settings.review)
        floor_refusal = automerge_floor_refusal(resolved_posture)
    except Exception as exc:  # noqa: BLE001 - a floor verdict fails closed
        return GrantVerdict(
            UNKNOWN,
            f"review posture unreadable ({type(exc).__name__}: {exc}); refusing "
            "to execute against an unverifiable floor",
            node_id=node_id,
            grant=receipt,
            claim_state=claim_state,
        )
    if floor_refusal:
        return GrantVerdict(
            HELD,
            floor_refusal,
            node_id=node_id,
            grant=receipt,
            claim_state=claim_state,
        )
    return GrantVerdict(
        GRANTED,
        f"newest durable grant approved (source: {receipt['source']}, recorded "
        f"{newest_stamp}), claim {claim_state}, live config grants dispatch, "
        f"posture {resolved_posture.value} clears the floor",
        node_id=node_id,
        grant=receipt,
        claim_state=claim_state,
    )
