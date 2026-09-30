"""Transport for the ``publish-review`` verb: one JSON payload in, one parsed
answer out. The producer lives in the Rust binary; this is the Python door.
"""

from __future__ import annotations

from typing import Any

from fno.rust_binary import VerbUnavailable


class PublishReviewUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def publish_review_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip; the timeout sits above the 30s resolver
    default because the verb makes real network round trips."""
    # Call-time import: a module-top capture can permanently hold a test's
    # monkeypatched verb_call stub, leaking it into every later reader.
    from fno.rust_binary import verb_call

    return verb_call("publish-review", payload, PublishReviewUnavailable, timeout=45)
