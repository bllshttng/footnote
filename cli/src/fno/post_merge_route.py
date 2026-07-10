"""Warm-session routing for the post-merge ritual.

When a PR merges, the session that opened it (the node's
``source_session_id``) still holds the context a cold worker would have to
re-derive. Both merge detectors (the pr-watch daemon and the reconcile
backstop) route through :func:`fno.graph._reconcile.dispatch_post_merge_ritual`,
which calls this module to try a live inject into that originating session
before falling back to the cold dispatch.

Deliberately a leaf: imports of ``fno.agents`` / ``fno.relay`` stay
function-local so the graph/pr_watch import graphs pick up nothing new.
"""
from __future__ import annotations

import os
from typing import Optional, Tuple

from fno.harness_identity import HARNESS_SESSION_MARKERS

# The injected turn. `autonomous` rides the prompt because neither route has an
# operator guaranteed present (same contract as the cold dispatch sites).
WARM_PROMPT = "/fno:pr merged {pr} autonomous"

# The self-inject guard follows the same shared precedence used to stamp node
# provenance, plus the legacy Claude marker for compatibility.
_SELF_SESSION_ENV_VARS = tuple(marker for marker, _ in HARNESS_SESSION_MARKERS) + (
    "CLAUDE_SESSION_ID",
)


def _current_session_ids() -> set[str]:
    """The running session's own id(s) from the ambient env (non-empty only)."""
    return {v for k in _SELF_SESSION_ENV_VARS if (v := (os.environ.get(k) or "").strip())}


def resolve_warm_session(source_session_id: Optional[str]) -> Optional[str]:
    """Map a node's originating session id to a live local CC session id.

    Returns the session id only when a live, identity-checked (pid +
    ``procStart`` create-time) local session matches. ``None`` for a missing
    id, the currently-running session (never self-inject the ritual into the
    session executing this code), a dead/reused pid, or any resolver error --
    every ``None`` means "take the cold path".
    """
    sid = (source_session_id or "").strip()
    if not sid:
        return None
    if sid in _current_session_ids():
        return None
    try:
        from fno.agents.discover import discover_live_sessions

        for s in discover_live_sessions():
            if s.session_id == sid:
                return sid
    except Exception:
        return None
    return None


def inject_pr_merged(session_id: str, pr_number: int) -> Tuple[bool, str]:
    """Live-inject the ritual command into ``session_id``. Never raises.

    Returns ``(delivered, reason)``. A busy recipient queues the injected turn
    (answer-queue semantics); when it is not recorded within the inject's
    growth-confirm budget the outcome is ``queue-timeout`` and the caller cold
    dispatches -- the queued turn may still land later, the same bounded
    double-delivery the mail-inject vehicle already documents and accepts.
    """
    try:
        from fno.relay.roundtrip import (
            INJECT_CONFIRMED,
            INJECT_UNCONFIRMED,
            submit_via_control_reply,
        )

        outcome = submit_via_control_reply(
            session_id, WARM_PROMPT.format(pr=pr_number)
        )
    except Exception as exc:  # inject failure is a routing signal, never fatal
        return False, f"inject-error: {exc}"[:120]
    if outcome == INJECT_CONFIRMED:
        return True, "delivered"
    if outcome == INJECT_UNCONFIRMED:
        return False, "queue-timeout"
    return False, "not-live"
