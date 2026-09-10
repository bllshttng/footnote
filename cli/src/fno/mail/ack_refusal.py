"""Why a bus row refused the ack.

Every non-deliverable shape gets one line naming what the row actually is.
Each refusal must never make the claim its transport cannot back: a typed row
never claims confirmation, a landed row is never mistaken for mail (x-22ce).
"""
from __future__ import annotations

from fno.bus.log import LANDED_KIND, TYPED_DELIVERY, Envelope


def refusal_line(target: Envelope) -> str:
    """The stderr line for refusing to ack a non-deliverable row."""
    if target.kind == LANDED_KIND:
        acked = (target.meta or {}).get("landed")
        return (
            f"message {target.id!r} is a landed receipt, not mail; "
            f"it acknowledges {acked!r}; cursor not advanced"
        )
    if target.delivery == TYPED_DELIVERY:
        how = "typed into a pane (delivery unconfirmed)"
    else:
        how = "already delivered (hosted)"
    return f"message {target.id!r} was {how}; cursor not advanced"
