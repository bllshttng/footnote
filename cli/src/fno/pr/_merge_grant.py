"""Typed durable-grant resolver: one answer for status, merge, and the watcher.

The verdict is owned by ``crates/fno-agents/src/merge_grant.rs``, reached
through the ``authorized-merge`` verb's ``grant-verdict`` op. This module is
the transport, so status, the merge verb and the watcher read one owner and
there is exactly one precedence order.
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


def resolve_durable_grant(pr_number: int, repo: str) -> GrantVerdict:
    """Resolve the durable merge verdict for the node this PR delivers.

    A transport, never a decision: the Rust owner answers, and an unreachable
    owner reads ``unknown`` - an unread grant never grants.
    """
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        out = verb_call(
            "authorized-merge",
            {"op": "grant-verdict", "pr": int(pr_number), "cwd": repo},
        )
    except VerbUnavailable as exc:
        return GrantVerdict(
            UNKNOWN,
            f"the durable-grant resolver could not be reached ({exc}); "
            "an unread grant never grants",
        )
    state = out.get("state")
    return GrantVerdict(
        str(state) if state else UNKNOWN,
        str(out.get("reason") or ""),
        node_id=out.get("node_id"),
        grant=out.get("grant"),
        claim_state=out.get("claim_state"),
    )
