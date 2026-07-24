"""The post-merge ritual's one dispatch seam: route, receipt, and execution.

When a PR merges, the session that shipped it still holds the context a cold
runner would have to re-derive. The pr-watch daemon is the sole merge detector;
it calls :func:`dispatch_post_merge_ritual` here, which asks
:func:`decide_post_merge_route` for the single warm/cold/defer verdict, reserves
a durable receipt, and then either live-injects the ritual into the borrowed
session or runs ``fno pr ritual <n> --autonomous`` directly. No post-merge path
creates a background thread.

The module owns the decision so a later caller adopts one function instead of
extracting the choice from three: callers supply facts and execute the verdict,
and none of them probes a session and picks a route on its own.

Deliberately a leaf: imports of ``fno.agents`` / ``fno.relay`` / ``fno.config``
stay function-local so the graph/pr_watch import graphs pick up nothing new.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Literal, Optional, Tuple

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


# ---------------------------------------------------------------------------
# The one warm/cold/defer decision
# ---------------------------------------------------------------------------

Route = Literal["warm", "cold", "defer"]


@dataclass(frozen=True)
class RouteDecision:
    """The single verdict every post-merge caller executes.

    ``delivering_*`` names the session that SHIPPED the PR; ``borrowed_*`` names
    the live session the ritual is injected into. They are usually the same, and
    the case where they differ is the one that was invisible before.
    """

    route: Route
    reason: str
    delivering_session_id: Optional[str] = None
    delivering_harness: Optional[str] = None
    borrowed_session_id: Optional[str] = None
    borrowed_harness: Optional[str] = None
    # Gates the cold path's direct-finalize rung: a live origin is never
    # finalized, so this is only ever True alongside ``route == "cold"``.
    origin_dead: bool = False


def decide_post_merge_route(
    *,
    auto_run: bool,
    ship_session_id: Optional[str] = None,
    ship_harness: Optional[str] = None,
    source_session_id: Optional[str] = None,
    source_harness: Optional[str] = None,
    source_cwd: Optional[str] = None,
    resolve_warm: Optional[Callable[[Optional[str], Optional[str]], Optional[str]]] = None,
    death_confirmed: Optional[Callable[..., bool]] = None,
) -> RouteDecision:
    """Decide warm, cold, or defer for one merged PR. The only such function.

    Order: an explicit automatic-run disable defers; otherwise the latest
    ``phase: ship`` identity is tried first (it names who actually delivered the
    PR), then the node-birth ``source_session_id`` for pre-provenance nodes; a
    reachable candidate is warm, and no reachable candidate is cold.

    Never reads receipt history -- the dispatch marker and claims are the only
    idempotency inputs, and folding attribution into that decision would make
    the receipt a hidden third guard with its own failure semantics.
    """
    if not auto_run:
        return RouteDecision("defer", "auto-run-disabled")

    _resolve = resolve_warm if resolve_warm is not None else resolve_warm_session
    _dead = death_confirmed if death_confirmed is not None else session_death_confirmed

    ship_sid = (ship_session_id or "").strip() or None
    source_sid = (source_session_id or "").strip() or None
    provenance = "ship-provenance" if ship_sid else "no-ship-provenance"

    candidates: list[tuple[str, Optional[str]]] = []
    for sid, harness in ((ship_sid, ship_harness), (source_sid, source_harness)):
        if sid and sid not in {c[0] for c in candidates}:
            candidates.append((sid, harness))

    cold_reason = "no-live-source-session"
    try:
        for sid, harness in candidates:
            warm_sid = _resolve(sid, harness)
            if warm_sid is not None:
                return RouteDecision(
                    "warm",
                    provenance,
                    delivering_session_id=ship_sid,
                    delivering_harness=ship_harness if ship_sid else None,
                    borrowed_session_id=warm_sid,
                    borrowed_harness=harness,
                )
    except Exception as exc:  # noqa: BLE001 - a resolver fault routes cold, never raises
        cold_reason = f"warm-error: {exc}"[:120]

    # Cold. The direct-finalize rung needs an explicit family-1 death verdict for
    # the legacy origin (whose manifest and transcript it reads from disk), not
    # merely "not warm-reachable".
    origin_dead = False
    if source_sid and source_cwd:
        try:
            origin_dead = bool(_dead(source_sid, source_harness, source_cwd))
        except Exception:  # noqa: BLE001 - an unprovable death is not a death
            origin_dead = False

    if not ship_sid:
        cold_reason = f"{cold_reason}; {provenance}"
    return RouteDecision(
        "cold",
        cold_reason,
        delivering_session_id=ship_sid,
        delivering_harness=ship_harness if ship_sid else None,
        origin_dead=origin_dead,
    )


# ---------------------------------------------------------------------------
# The durable attribution receipt
# ---------------------------------------------------------------------------

RECEIPT_EVENT = "post_merge_dispatch_receipt"
_DETAIL_MAX = 512


def _receipt_events_path() -> Optional[Path]:
    """The global events log. Launchd starts pr-watch with no working directory,
    so a cwd-relative path would silently drop every receipt."""
    try:
        from fno.paths import state_dir

        return state_dir() / "events.jsonl"
    except Exception:  # noqa: BLE001
        return None


def emit_receipt(
    phase: Literal["reserved", "accepted", "failed", "deferred"],
    *,
    dispatch_id: str,
    attempt_id: str,
    pr: int,
    route: str,
    outcome: str,
    node_id: Optional[str] = None,
    repo_slug: Optional[str] = None,
    detector: str = "pr-watch",
    delivering_session_id: Optional[str] = None,
    delivering_harness: Optional[str] = None,
    borrowed_session_id: Optional[str] = None,
    borrowed_harness: Optional[str] = None,
    detail: Optional[str] = None,
    events_path: Optional[Path] = None,
) -> bool:
    """Append one post-merge dispatch receipt. Returns False on any write failure.

    The caller decides what a False means: a ``reserved`` write is fail-closed
    (no work starts without its artifact), every later phase is best-effort
    because the work has already landed and a second delivery would be worse
    than a missing record.
    """
    data: dict[str, Any] = {
        "dispatch_id": dispatch_id,
        "attempt_id": attempt_id,
        "phase": phase,
        "pr": int(pr),
        "detector": detector,
        "route": route,
        "outcome": outcome,
    }
    # Absent identities are omitted rather than written empty: an invented
    # attribution is worse than a recorded miss.
    for key, value in (
        ("node_id", node_id),
        ("repo_slug", repo_slug),
        ("delivering_session_id", delivering_session_id),
        ("delivering_harness", delivering_harness),
        ("borrowed_session_id", borrowed_session_id),
        ("borrowed_harness", borrowed_harness),
        ("detail", (detail or "")[:_DETAIL_MAX] or None),
    ):
        if value:
            data[key] = value

    path = events_path if events_path is not None else _receipt_events_path()
    if path is None:
        return False
    try:
        from fno.events import _build, append_event

        append_event(_build(RECEIPT_EVENT, "daemon", data), path)
    except Exception:  # noqa: BLE001 - the caller's phase decides what a miss means
        return False
    return True


def new_attempt_id() -> str:
    """A fresh correlation key for one reserve/action/result attempt. A crash
    retry gets a new one under the same ``dispatch_id``."""
    return uuid.uuid4().hex[:12]
