"""Transport for the ``publish-review`` verb: one JSON payload in, one parsed
answer out. The producer itself lives in the Rust binary
(``crates/fno-agents/src/publish_review.rs``); this module is the Python door
the emit chokepoint and the hidden ``fno pr publish-review`` verb share.
"""

from __future__ import annotations

from typing import Any

from fno.rust_binary import VerbUnavailable, verb_call


class PublishReviewUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def publish_review_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip with the bot-review producer.

    ``timeout`` is raised above the 30s resolver default: the verb makes real
    network round trips (a gh read, the POST, the reviewDecision readback), so
    the caller's bound must not report an owner unreachable for a decision
    that was merely still running.
    """
    return verb_call("publish-review", payload, PublishReviewUnavailable, timeout=45)
