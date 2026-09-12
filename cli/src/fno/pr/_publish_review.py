"""Transport for the ``publish-review`` verb: one JSON payload in, one parsed
answer out. The producer lives in the Rust binary; this module is the Python
door the emit chokepoint and the hidden verb share.
"""

from __future__ import annotations

from typing import Any

from fno.rust_binary import VerbUnavailable, verb_call


class PublishReviewUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def publish_review_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip; the timeout sits above the 30s resolver
    default because the verb makes real network round trips."""
    return verb_call("publish-review", payload, PublishReviewUnavailable, timeout=45)
