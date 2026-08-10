"""Resolve a job address (``node:<id>`` / ``pr:<n>``) to the current claim holder.

A job address names the WORK, not the process holding it. It resolves at
delivery time to whoever holds the ``node:<id>`` claim RIGHT NOW, so the address
outlives any one session: when a holder dies and a successor re-claims the node,
the successor's drain picks up mail the dead session never read. That dissolves
the dead-handle strand (x-8f8c part 2): a mail address used to name a process
that dies, so the address expired faster than the message.

Resolution is a read over the EXISTING claim system -- ``claim_status`` already
returns the holder of ``node:<id>``. This module adds no new subsystem; it maps
two recipient tokens to that read and reports the state so a sender can refuse
rather than queue (a job address with no holder would strand at the new address,
reproducing the defect it exists to fix).

``pr:<n>`` is sugar: it resolves to the ``node:<id>`` whose PR is ``n`` via the
backlog graph, then becomes that node address for every downstream purpose. The
durable envelope is addressed to ``node:<id>`` either way, so the drain has one
address space to consume.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


# Claim states that mean "a holder exists right now" -- live (pid alive) and
# suspect (TTL-unexpired, dead pid: a respawned worker's protected slot). A
# message addressed to the job is deliverable to either; everything else
# (free / stale / corrupted) means no holder and must NOT queue.
HOLDER_STATES = frozenset({"live", "suspect"})


@dataclass(frozen=True)
class JobHolder:
    """The result of resolving a job address.

    ``address`` is always ``node:<id>`` (the canonical durable address) once
    resolved; a ``pr:<n>`` input is normalized to the node it maps to.
    ``session_id`` is the holder's harness session id for live-inject, or None
    when no holder exists. ``harness`` routes the inject (claude vs codex).
    """

    node_id: str
    address: str
    state: str
    session_id: Optional[str]
    harness: Optional[str]
    # Why resolution landed where it did -- surfaced in the refusal so a sender
    # can tell "no such node" from "node exists, claim free" from "pr not found".
    note: Optional[str] = None

    @property
    def has_holder(self) -> bool:
        return self.state in HOLDER_STATES and self.session_id is not None


def is_job_token(token: str) -> bool:
    """True for a ``node:<id>`` or ``pr:<n>`` recipient token."""
    return token.startswith("node:") or token.startswith("pr:")


def _session_from_holder(holder: Optional[str]) -> Optional[str]:
    """Strip the ``target-session:`` prefix to recover the holder's session id.

    Mirrors ``fno.agents.truth_status._session_from_holder`` (the one consumer of
    this join today). A claim holder is ``target-session:<session-id>``; for
    claude that id is exactly what mail-inject targets. Codex claims can be owned
    by the durable thread id, so a codex holder is a known approximation until the
    manifest join refines it -- and the dominant, testable case is claude.
    """
    if holder and holder.startswith("target-session:"):
        sid = holder[len("target-session:"):]
        return sid or None
    return None


def _node_id_for_pr(pr_number: int) -> Optional[str]:
    """Find the backlog node id whose PR is ``pr_number``.

    Prefers a node with a live/suspect claim (the active worker for that PR);
    falls back to the first node carrying the PR so a caller can report "no live
    holder" rather than "no such PR". The graph is the cross-project backlog, so
    a bare number can match more than one repo -- the active-claim preference is
    the tiebreak (only one session holds a node at a time).
    """
    from fno.graph.store import read_graph

    fallback: Optional[str] = None
    for node in read_graph():
        if not isinstance(node, dict):
            continue
        if node.get("pr_number") != pr_number:
            continue
        nid = node.get("id")
        if not isinstance(nid, str):
            continue
        if fallback is None:
            fallback = nid
        # An active claim wins outright.
        res = _resolve_node(nid)
        if res.has_holder:
            return nid
    return fallback


def _resolve_node(node_id: str) -> JobHolder:
    """Resolve a ``node:<id>`` to its claim holder over the global claims root."""
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    key = f"node:{node_id}"
    root = claims_root_for(key)
    status = claim_status(key, root=root)
    return JobHolder(
        node_id=node_id,
        address=key,
        state=status.get("state") or "free",
        session_id=_session_from_holder(status.get("holder")),
        harness=status.get("harness"),
        note=None,
    )


def resolve_job_address(token: str) -> Optional[JobHolder]:
    """Resolve a ``node:<id>`` / ``pr:<n>`` token to the current holder, or None.

    None means the token is not a job address (the caller treats it as a normal
    session/name recipient). A returned ``JobHolder`` with ``has_holder`` False is
    a job address with no current holder -- the caller refuses rather than queue.
    """
    if token.startswith("node:"):
        node_id = token[len("node:"):].strip()
        if not node_id:
            return None
        return _resolve_node(node_id)
    if token.startswith("pr:"):
        raw = token[len("pr:"):].strip()
        if not raw or not raw.isdigit():
            return JobHolder(
                node_id="",
                address=token,
                state="free",
                session_id=None,
                harness=None,
                note=f"pr address must be pr:<number> (got {token!r})",
            )
        nid = _node_id_for_pr(int(raw))
        if nid is None:
            return JobHolder(
                node_id="",
                address=token,
                state="free",
                session_id=None,
                harness=None,
                note=f"no backlog node carries PR {raw}",
            )
        res = _resolve_node(nid)
        return JobHolder(
            node_id=res.node_id,
            address=res.address,
            state=res.state,
            session_id=res.session_id,
            harness=res.harness,
            note=f"pr:{raw} -> {res.address}",
        )
    return None
