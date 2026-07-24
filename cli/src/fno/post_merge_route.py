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


def _live_codex_registry_entry(session_id: str):
    """A codex registry candidate addressable as ``session_id``, preferring one
    that carries a live transport (``mux``), or ``None``.

    v10 (x-880e): every codex row records its id in the canonical
    ``harness_session_id`` (a mux_spawn pane row carries it plus a ``mux`` ref; a
    SessionStart-registered row carries it with no transport). Both match on that
    one field, so the transportless row alone would make ``_deliver_live`` fall
    through to the daemon path and cold-spawn instead of PaneSending into the
    panel (codex peer P2, PR #328). So prefer the transport-bearing (``mux``) row.
    Function-local import keeps this a leaf."""
    try:
        from fno.agents.registry import load_registry

        matches = [
            e
            for e in load_registry()
            if getattr(e, "harness", None) == "codex"
            and session_id == getattr(e, "harness_session_id", None)
        ]
    except Exception:
        return None
    if not matches:
        return None
    return next((e for e in matches if getattr(e, "mux", None)), matches[0])


def _family1_state(
    session_id: str,
    entry=None,
    source_cwd: Optional[str] = None,
    source_harness: str = "claude",
) -> str:
    """Return transcript truth for an origin candidate; never infer from status."""
    from types import SimpleNamespace

    from fno.agents.discover import default_projects_dir
    from fno.agents.session_truth import resolve_session_truth

    if entry is None and not source_cwd:
        result = resolve_session_truth(session_id)
    else:
        known = SimpleNamespace(
            agent=(
                getattr(entry, "harness", None)
                if entry is not None
                else source_harness
            ),
            session_id=session_id,
            cwd=(getattr(entry, "cwd", "") if entry is not None else source_cwd) or "",
        )
        result = resolve_session_truth(
            session_id,
            resolve=lambda _handle: (known, []),
            projects_root=default_projects_dir(),
        )
    return str(result.get("state") or "unknown")


def session_death_confirmed(
    source_session_id: Optional[str],
    source_harness: Optional[str] = None,
    source_cwd: Optional[str] = None,
) -> bool:
    """True only for an explicit family-1 ``done`` or ``stalled`` verdict."""
    sid = (source_session_id or "").strip()
    if not sid:
        return False
    harness = (source_harness or "claude").strip().lower()
    entry = _live_codex_registry_entry(sid) if harness == "codex" else None
    return _family1_state(sid, entry, source_cwd, harness) in {"done", "stalled"}


def resolve_warm_session(
    source_session_id: Optional[str], source_harness: Optional[str] = None
) -> Optional[str]:
    """Map a node's originating ``(session_id, harness)`` to a live, reachable
    peer, or ``None`` to take the cold path.

    ``source_harness`` selects the liveness probe: ``claude`` (default) matches a
    live local CC session via disk discovery; ``codex`` matches a live codex row
    in the agent registry holding this threadId (the shipping panel). ``gemini``
    has no live-inject vehicle yet (US9) so it always cold-paths. A missing id,
    the currently-running session (never self-inject), or any resolver error is
    ``None``.
    """
    sid = (source_session_id or "").strip()
    if not sid:
        return None
    if sid in _current_session_ids():
        return None
    harness = (source_harness or "claude").strip().lower()
    if harness == "claude":
        try:
            from fno.agents.discover import discover_live_sessions

            for s in discover_live_sessions():
                if getattr(s, "is_alive", True) and s.session_id == sid:
                    return sid
        except Exception:
            return None
        return None
    if harness == "codex":
        entry = _live_codex_registry_entry(sid)
        if entry is None:
            return None
        state = _family1_state(sid, entry)
        return sid if state in {"working", "watching", "your-move"} else None
    # gemini / unknown harness: no live-inject vehicle yet -> cold path.
    return None


def inject_pr_merged(
    session_id: str, pr_number: int, source_harness: Optional[str] = None
) -> Tuple[bool, str]:
    """Live-inject the ritual command into the originating peer. Never raises.

    Injects the RAW ``/fno:pr merged`` command (NOT an ``<fno_mail>`` envelope)
    so the peer EXECUTES it rather than treating it as chat. ``claude`` uses the
    control.sock reply (a busy recipient queues the turn -> ``queue-timeout`` ->
    cold dispatch, the queued turn may still land later: a bounded double-
    delivery the vehicle already accepts). ``codex`` reaches its live panel via
    the shared ``_deliver_live`` vehicle (``_mux_pane_send`` for a mux pane, the
    daemon RPC otherwise) with ``mail=None`` so the command lands verbatim. Any
    miss returns ``(False, reason)`` and the caller cold-dispatches.
    """
    command = WARM_PROMPT.format(pr=pr_number)
    harness = (source_harness or "claude").strip().lower()
    if harness == "claude":
        try:
            from fno.relay.roundtrip import (
                INJECT_CONFIRMED,
                INJECT_UNCONFIRMED,
                submit_via_control_reply,
            )

            outcome = submit_via_control_reply(session_id, command)
        except Exception as exc:  # inject failure is a routing signal, never fatal
            return False, f"inject-error: {exc}"[:120]
        if outcome == INJECT_CONFIRMED:
            return True, "delivered"
        if outcome == INJECT_UNCONFIRMED:
            return False, "queue-timeout"
        return False, "not-live"
    if harness == "codex":
        entry = _live_codex_registry_entry(session_id)
        if entry is None:
            return False, "not-live"
        try:
            from fno.agents.dispatch import _deliver_live

            delivered = _deliver_live(entry, command, from_name="fno", mail=None)
        except Exception as exc:
            return False, f"inject-error: {exc}"[:120]
        return (True, "delivered") if delivered else (False, "not-live")
    return False, f"unsupported-harness:{harness}"
