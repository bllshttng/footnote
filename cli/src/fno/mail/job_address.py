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
    """Re-exported from :mod:`fno.agents.truth_status` (the single source).

    A claim holder is ``target-session:<session-id>``; for claude that id is
    exactly what mail-inject targets. Codex claims can be owned by the durable
    thread id, so a codex holder is a known approximation until the manifest join
    refines it -- and the dominant, testable case is claude.
    """
    from fno.agents.truth_status import _session_from_holder as _impl

    return _impl(holder)


def _node_ids_for_pr(pr_number: int) -> list[str]:
    """Every backlog node id carrying PR ``pr_number`` (primary OR additional).

    PR numbers are per-repo, so the cross-project graph can carry the same number
    under more than one node: the caller refuses on ambiguity rather than
    silently routing ``pr:<n>`` to the wrong project's holder. Reuses
    ``_node_carries_pr`` so the primary + ``additional_prs`` contract stays the
    graph module's, not a restatement here.
    """
    from fno.graph.store import _node_carries_pr, read_graph

    out: list[str] = []
    for node in read_graph():
        if not isinstance(node, dict):
            continue
        nid = node.get("id")
        if isinstance(nid, str) and _node_carries_pr(node, pr_number):
            out.append(nid)
    return out


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
        # isdecimal, not isdigit: the latter is True for unicode digits
        # (superscripts, subscripts) that int() rejects, which would crash the
        # path instead of refusing cleanly. A try/except catches anything isdecimal
        # misses too.
        if not raw or not raw.isdecimal():
            return JobHolder(
                node_id="",
                address=token,
                state="free",
                session_id=None,
                harness=None,
                note=f"pr address must be pr:<number> (got {token!r})",
            )
        try:
            pr_num = int(raw)
        except ValueError:
            return JobHolder(
                node_id="",
                address=token,
                state="free",
                session_id=None,
                harness=None,
                note=f"pr address must be pr:<number> (got {token!r})",
            )
        candidates = _node_ids_for_pr(pr_num)
        if not candidates:
            return JobHolder(
                node_id="",
                address=token,
                state="free",
                session_id=None,
                harness=None,
                note=f"no backlog node carries PR {raw}",
            )
        if len(candidates) > 1:
            # PR numbers are per-repo; >1 node carrying the same number means the
            # bare pr:<n> is ambiguous across projects. Refuse rather than silently
            # route to one -- the sender disambiguates with node:<id>.
            return JobHolder(
                node_id="",
                address=token,
                state="free",
                session_id=None,
                harness=None,
                note=(
                    f"PR {raw} is ambiguous across nodes {', '.join(candidates)}; "
                    f"use node:<id> to pick one"
                ),
            )
        res = _resolve_node(candidates[0])
        return JobHolder(
            node_id=res.node_id,
            address=res.address,
            state=res.state,
            session_id=res.session_id,
            harness=res.harness,
            note=f"pr:{raw} -> {res.address}",
        )
    return None
