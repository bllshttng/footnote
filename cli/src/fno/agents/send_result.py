"""The return contract of :func:`fno.agents.dispatch.dispatch_send`; split from
dispatch.py (file-budget: shrink-only), which re-exports the name."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


@dataclass
class DispatchSendResult:
    """Return shape for :func:`dispatch_send`; field notes ride the fields."""

    msg_id: str
    delivery: str  # "hosted" | "durable"
    # The live lane's own cause when delivery demoted to durable (node x-1904):
    # the claude control.sock vocabulary (not-confirmed / attach-failed / ...),
    # a codex RPC reason, or a mux token. None when no live attempt ran (the
    # recipient was asleep, so durable was written upfront with no live miss).
    reason: Optional[str] = None
    # Set by the --to-project anycast path (resolve_to_project): the registry
    # name the project resolved to (when one live peer), and the destination
    # project (for the durable-queue and resolved-recipient stdout lines).
    recipient: Optional[str] = None
    to_project: Optional[str] = None
    # Owner class the durable write was stamped with (x-1602); None if none was.
    durable_owner: Optional[str] = None
