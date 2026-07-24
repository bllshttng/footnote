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

# The injected turn: the SAME mechanical verb the cold path runs as a subprocess
# (Locked Decision 11). Warm and cold execute one identical bounded command and
# differ only in transport (live inject vs. daemon subprocess); the verb's own
# conditional headless judgment leg is the single model-capable layer on both,
# so a borrowed live session cannot re-derive the ritual unbounded (the PR #575
# -> #577 failure mode this closes).
WARM_PROMPT = "fno pr ritual {pr} --autonomous"

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
    """Live-inject the ritual verb into the originating peer. Never raises.

    Injects the RAW ``WARM_PROMPT`` command - the same ``fno pr ritual <pr>
    --autonomous`` verb the cold path runs as a subprocess, NOT an
    ``<fno_mail>`` envelope, so the peer EXECUTES it rather than treating it as
    chat. ``claude`` uses the control.sock reply (a busy recipient queues the
    turn -> ``queue-timeout`` -> cold dispatch, the queued turn may still land
    later: a bounded double-delivery the vehicle already accepts). ``codex``
    reaches its live panel via the shared ``_deliver_live`` vehicle
    (``_mux_pane_send`` for a mux pane, the daemon RPC otherwise) with
    ``mail=None`` so the command lands verbatim. Any miss returns
    ``(False, reason)`` and the caller cold-dispatches.
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


# ---------------------------------------------------------------------------
# Dispatch: marker + claim dedup, receipt, and the warm/cold/defer execution
# ---------------------------------------------------------------------------

# The cross-session / cross-trigger marker recording that the ritual already ran
# for one merge SHA. Retained through the seven-day observation window; the TTL
# claim below is the single layer that survives the eventual cleanup.
_POST_MERGE_DISPATCH_SUBDIR = "post-merge-dispatched"
_POST_MERGE_DISPATCH_TTL_MS = 15 * 60 * 1000


def _dispatch_marker(canonical: Path, key: str) -> Path:
    return canonical / ".fno" / _POST_MERGE_DISPATCH_SUBDIR / key


@dataclass(frozen=True)
class PostMergeDispatchResult:
    """Outcome of one dispatch attempt. pr-watch branches on ``outcome``."""

    outcome: str  # disabled | already-dispatched | routed-warm | dispatched | finalized-origin | failed
    pr_number: int
    short_id: str = ""
    detail: Optional[str] = None


def _origin_transcript_path(
    session_id: Optional[str], cwd: Optional[str], harness: Optional[str]
) -> Optional[Path]:
    """The on-disk claude transcript for a ``(session, cwd)`` origin, or None.

    Liveness-independent pure-filesystem existence (NOT ``discover_live_sessions``,
    which keys on a live process - exactly what a dead origin lacks). Claude-first:
    a non-claude harness yields None, so a codex/gemini origin falls through to
    cold + the backstop."""
    if not session_id or not cwd:
        return None
    if harness and harness != "claude":
        return None
    try:
        from fno.agents.discover import _candidate_dir_names, default_projects_dir

        projects = default_projects_dir()
        names = list(_candidate_dir_names(cwd))
    except Exception:  # noqa: BLE001 - discover unavailable -> no probe
        return None
    for name in names:
        candidate = projects / name / f"{session_id}.jsonl"
        if candidate.is_file():
            return candidate
    return None


def origin_transcript_exists(
    session_id: Optional[str], cwd: Optional[str], harness: Optional[str]
) -> bool:
    """True iff the origin's transcript AND its ``target-state.md`` both survive.

    The direct-finalize rung gate: finalize reads manifest + transcript from disk,
    so both must exist for a full-fidelity row."""
    if cwd is None or _origin_transcript_path(session_id, cwd, harness) is None:
        return False
    return (Path(cwd) / ".fno" / "target-state.md").is_file()


def _finalize_origin_ledger(
    source_cwd: str, transcript: str, harness: Optional[str]
) -> bool:
    """Invoke ``fno-agents finalize`` against a dead origin's manifest+transcript.

    Writes the full-fidelity ledger row with no session revival. ``--reason
    DoneAwaitingMerge`` is NOT a SHIP_REASON, so finalize runs the always-branch
    ledger row only, never re-running plan-stamp/handoff against the dead origin."""
    try:
        from fno.agents.rust_runtime import resolve_binary

        binary = resolve_binary()
    except Exception:  # noqa: BLE001 - runtime resolver unavailable
        binary = None
    if binary is None:
        return False
    state = Path(source_cwd) / ".fno" / "target-state.md"
    cmd = [
        str(binary), "finalize",
        "--state", str(state),
        "--cwd", str(source_cwd),
        "--reason", "DoneAwaitingMerge",
        "--transcript", str(transcript),
    ]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    except (OSError, subprocess.SubprocessError):
        return False
    return proc.returncode == 0


@dataclass(frozen=True)
class ColdRitualResult:
    """Outcome of one bounded ``fno pr ritual <n> --autonomous`` subprocess run.

    ``tail`` is the bounded stdout receipt (the verb prints a per-leg
    ``step=<name> status=<ok|skipped|failed>`` line) carried into the dispatch
    receipt's detail for forensics."""

    ok: bool
    tail: str = ""


def _default_run_ritual_verb(pr_number: int, cwd: str) -> ColdRitualResult:
    """Run ``fno pr ritual <n> --autonomous`` from the candidate canonical root.

    A bounded subprocess: the launchd tick never overlaps, so an unbounded verb
    would wedge every future tick (x-97d8). A non-zero exit is a dispatch failure
    the retry/park machinery handles. The verb owns its default-on conditional
    headless judgment leg, so this is the ONLY model-capable layer on the cold
    path - pr-watch adds none of its own (AC1-HP)."""
    try:
        from fno import _subprocess_util

        cmd = [
            *_subprocess_util.fno_py_cmd(),
            "pr", "ritual", str(pr_number), "--autonomous",
        ]
        proc = subprocess.run(
            cmd, capture_output=True, text=True, cwd=cwd, timeout=300
        )
    except (subprocess.SubprocessError, OSError) as exc:
        return ColdRitualResult(ok=False, tail=f"verb-error: {exc}"[:200])
    tail = (proc.stdout or "").strip()[-200:]
    return ColdRitualResult(ok=proc.returncode == 0, tail=tail)


def dispatch_post_merge_ritual(
    pr_number: int,
    *,
    dedup_key: Optional[str] = None,
    auto_run: bool = False,
    node_cwd: Optional[str] = None,
    canonical_root: Optional[Path] = None,
    ship_session_id: Optional[str] = None,
    ship_harness: Optional[str] = None,
    source_session_id: Optional[str] = None,
    source_harness: Optional[str] = None,
    source_cwd: Optional[str] = None,
    node_id: Optional[str] = None,
    repo_slug: Optional[str] = None,
    run_verb: Optional[Callable[[int, str], ColdRitualResult]] = None,
    warm_inject: Optional[Callable[[str, int, Optional[str]], Tuple[bool, str]]] = None,
    finalize_origin: Optional[Callable[[str, str, Optional[str]], bool]] = None,
    events_path: Optional[Path] = None,
    emit_receipt_fn: Optional[Callable[..., bool]] = None,
) -> PostMergeDispatchResult:
    """Hand one merged PR to the post-merge ritual, at most once per merge.

    The one warm/cold/defer verdict comes from :func:`decide_post_merge_route`;
    this function owns the marker + claim dedup (the only idempotency layer - the
    receipt never is), reserves a durable receipt before any action, then either
    live-injects the ritual verb into the borrowed session or runs
    ``fno pr ritual <n> --autonomous`` directly. No post-merge path creates a
    background thread.

    ``auto_run=False`` is a clean defer: one deferred receipt, no work, no marker.
    A warm inject that delivers or queues marks the merge and returns
    ``routed-warm``; a warm miss degrades to the cold verb. The cold path runs the
    direct-finalize ledger rung first when the origin is provably dead, then always
    falls through to the same verb invocation. A non-zero verb exit appends a
    ``failed`` receipt, writes no marker, and leaves the merge retryable.
    """
    _emit = emit_receipt_fn if emit_receipt_fn is not None else emit_receipt
    _path = events_path

    if canonical_root is None:
        from fno.paths import resolve_canonical_repo_root, resolve_canonical_worktree

        if node_cwd:
            wt = resolve_canonical_worktree(cwd=Path(node_cwd))
            canonical = Path(wt) if wt is not None else Path(node_cwd)
        else:
            canonical = Path(resolve_canonical_repo_root())
    else:
        canonical = Path(canonical_root)

    dispatch_id = re.sub(r"[^A-Za-z0-9._-]", "_", dedup_key or f"pr-{pr_number}")

    # Defer (auto_run off): one receipt, no marker, no work. Sits before the
    # marker/claim dedup so a disabled repo does not pay for a probe, and so the
    # receipt records the deliberate no-op rather than an already-dispatched skip.
    if not auto_run:
        _emit(
            "deferred",
            dispatch_id=dispatch_id, attempt_id=new_attempt_id(), pr=pr_number,
            route="defer", outcome="auto-run-disabled", node_id=node_id,
            repo_slug=repo_slug, detail="auto-run-disabled", events_path=_path,
        )
        return PostMergeDispatchResult("disabled", pr_number, detail="auto-run-disabled")

    marker = _dispatch_marker(canonical, dispatch_id)

    # Cross-session / cross-trigger dedup: a persisted marker means the ritual
    # already ran for this merge SHA (a completed no-op, not a route selector).
    if marker.exists():
        return PostMergeDispatchResult("already-dispatched", pr_number, detail="marker-exists")

    from fno import claims
    from fno.claims.io import claims_root_for

    def _persist_marker() -> None:
        try:
            marker.parent.mkdir(parents=True, exist_ok=True)
            marker.touch(exist_ok=True)
        except OSError:
            pass  # a missing marker only re-dispatches (idempotent ritual)

    # Re-entrancy guard: a live holder of the ritual's Step-0.5 mutex means the
    # verb is already running for this PR. Read via the GLOBAL claims root.
    ritual_key = f"reconcile:pr-{pr_number}"
    try:
        ritual_state = claims.claim_status(
            ritual_key, root=claims_root_for(ritual_key)
        ).get("state")
    except Exception:  # noqa: BLE001 - the guard must never break dispatch
        ritual_state = None
    if ritual_state == "live":
        _persist_marker()
        return PostMergeDispatchResult("already-dispatched", pr_number, detail="ritual-claim-live")
    if ritual_state == "suspect":
        # Crashed attended ritual: retry under a new attempt_id once the TTL clears.
        return PostMergeDispatchResult("already-dispatched", pr_number, detail="lock-contention")

    lock_key = f"post-merge-ritual:{dispatch_id}"
    holder = f"post-merge-dispatch:{pr_number}"
    try:
        claims.acquire_claim(
            lock_key, holder, ttl_ms=_POST_MERGE_DISPATCH_TTL_MS,
            reason="post-merge ritual dispatch", root=canonical,
        )
    except claims.ClaimHeldByOther:
        # In-flight, NOT done: another holder may still fail before the marker.
        return PostMergeDispatchResult("already-dispatched", pr_number, detail="lock-contention")

    try:
        if marker.exists():  # double-checked under the lock
            return PostMergeDispatchResult("already-dispatched", pr_number, detail="marker-exists")

        verdict = decide_post_merge_route(
            auto_run=True,
            ship_session_id=ship_session_id,
            ship_harness=ship_harness,
            source_session_id=source_session_id,
            source_harness=source_harness,
            source_cwd=source_cwd,
        )

        attempt = new_attempt_id()
        receipt_base: dict[str, Any] = dict(
            dispatch_id=dispatch_id, attempt_id=attempt, pr=pr_number,
            node_id=node_id, repo_slug=repo_slug,
            delivering_session_id=verdict.delivering_session_id,
            delivering_harness=verdict.delivering_harness,
            borrowed_session_id=verdict.borrowed_session_id,
            borrowed_harness=verdict.borrowed_harness,
            events_path=_path,
        )

        # Reserve the receipt BEFORE any action. Fail-closed: a write failure
        # starts no work and writes no marker, so the merge stays retryable.
        if not _emit("reserved", route=verdict.route, outcome="reserved", **receipt_base):
            return PostMergeDispatchResult("failed", pr_number, detail="receipt-reservation-failed")

        cold_reason = verdict.reason
        finalized_origin = False

        if verdict.route == "warm":
            _inject = warm_inject if warm_inject is not None else inject_pr_merged
            try:
                delivered, reason = _inject(
                    verdict.borrowed_session_id, pr_number, verdict.borrowed_harness
                )
            except Exception as exc:  # noqa: BLE001 - inject failure is a routing signal
                delivered, reason = False, f"inject-error: {exc}"[:120]
            if delivered or reason == "queue-timeout":
                queued = reason == "queue-timeout"
                _persist_marker()
                _emit(
                    "accepted", route="warm",
                    outcome="queued" if queued else "delivered", detail=reason,
                    **receipt_base,
                )
                return PostMergeDispatchResult(
                    "routed-warm", pr_number,
                    short_id=(verdict.borrowed_session_id or "")[:8],
                    detail="queued" if queued else reason,
                )
            cold_reason = reason  # warm miss -> degrade to the cold verb

        # Direct-finalize rung (cold prelude): a provably dead origin whose
        # manifest + transcript survive gets its full-fidelity ledger row written
        # directly, then control FALLS THROUGH to the verb (it does not short-
        # circuit: the ritual steps still run cold). A finalize failure degrades
        # to cold like a warm miss, naming the failure in the reason.
        if (
            verdict.origin_dead
            and source_cwd is not None
            and origin_transcript_exists(source_session_id, source_cwd, source_harness)
        ):
            _tpath = _origin_transcript_path(source_session_id, source_cwd, source_harness)
            if _tpath is not None:
                _finalize = (
                    finalize_origin if finalize_origin is not None else _finalize_origin_ledger
                )
                try:
                    finalized_origin = _finalize(source_cwd, str(_tpath), source_harness)
                except Exception as exc:  # noqa: BLE001 - degrade to cold, never break dispatch
                    finalized_origin = False
                    cold_reason = f"finalize-error: {exc}"[:120]

        _run = run_verb if run_verb is not None else _default_run_ritual_verb
        try:
            result = _run(pr_number, str(canonical))
        except Exception as exc:  # noqa: BLE001 - a runner fault is a dispatch failure
            result = ColdRitualResult(ok=False, tail=str(exc)[:200])
        if not result.ok:
            _emit(
                "failed", route="cold", outcome="verb-nonzero",
                detail=result.tail, **receipt_base,
            )
            return PostMergeDispatchResult(
                "failed", pr_number, detail=(result.tail[:200] or "verb-nonzero")
            )
        _persist_marker()
        _emit(
            "accepted", route="cold", outcome="completed",
            detail=(result.tail or None), **receipt_base,
        )
        return PostMergeDispatchResult(
            "finalized-origin" if finalized_origin else "dispatched",
            pr_number, short_id="verb", detail=f"cold: {cold_reason}",
        )
    finally:
        try:
            claims.release_claim(lock_key, holder, root=canonical)
        except Exception:  # noqa: BLE001 - TTL-bounded; a failed release self-heals
            pass
