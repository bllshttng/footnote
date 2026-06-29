"""fno.recovery — Layer-2 session auto-recovery watchdog (x-f47c).

A background ``/target`` session whose turn ends on an *abnormal* termination
(``API Error: Connection closed mid-response``, a dropped stream, an empty
response) is not resumed by anything today: the stream closed cleanly, so
Claude Code's stuck-stream watchdog treats it as not-stuck, and the footnote
stop hook / ``loop-check`` only ever fire on a clean Stop event. The work just
stops until the ~1h reaper kills the process, leaving the backlog node stuck
``claimed``. A human has to notice and type "keep going".

This module is the programmatic version of that "keep going": a periodic sweep
(hosted on the ``pr_watch`` launchd tick so it runs INDEPENDENTLY of any
session) that finds footnote-launched bg sessions which are idle-but-incomplete
and re-injects a resume nudge over Claude Code's messaging socket.

Design notes (the load-bearing parts):

- **Scope = registry ∩ live bg sessions.** Provenance comes from footnote's own
  agents registry (``provider == "claude"`` rows carry ``claude_short_id``);
  liveness + the messaging socket come from ``locate_session`` reading
  ``~/.claude/sessions``. The join is exactly "footnote-launched AND reachable",
  which satisfies the invariant *only ever touch sessions footnote launched*.
  Under-coverage (a footnote bg session missing from the registry) is the SAFE
  failure: we never nudge an arbitrary CC session, we just might miss one.

- **The predicate leans on freshness, not process-idle.** A clean
  connection-close leaves ``state.json`` frozen at its last value (often
  ``running``), indistinguishable from a busy session except that a busy
  session keeps *updating* ``state.json``. So ``classify`` nudges only when the
  state is non-terminal, not ``needs-input``, no ``<promise>`` is present, and
  ``updatedAt`` is staler than the idle threshold. Caveat: a session mid-way
  through a single long tool call (a multi-minute build/test) is busy but emits
  no turn events, so its ``state.json`` also freezes; the idle threshold
  default (15 min) is set above the longest expected single-tool runtime so
  such a session is not mistaken for wedged.

- **Reuse over re-implement.** The socket write is the shipped
  ``providers.claude.send_to_session`` (the same BG8 inject used by ``fno
  mail`` / dispatch); the state read is ``_claude_session_registry`` — this
  module is the predicate + the cadence wiring, nothing more.

ponytail: V1 handles the live-socket case (the observed repro — an idle-but-alive
session, socket still up). A suspended session (null socket) is skipped, not
respawned; the daemon-backend respawn-and-deliver path is the upgrade if
suspended-session recovery is ever observed in the wild.
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterable, Optional

from fno.agents.providers.claude import ProviderSocketError

# The error the real send seam raises; aliased so callers/tests have one name.
_SendError = ProviderSocketError

# Decision constants returned by :func:`classify`.
NUDGE = "nudge"
SKIP_NEEDS_INPUT = "skip-needs-input"
SKIP_TERMINAL = "skip-terminal"
SKIP_DONE = "skip-done"
NOT_STALE = "not-stale"

# States that mean "finished, never re-nudge". ``needs-input`` is handled
# separately (it emits a skipped event; the others are silent).
_TERMINAL = frozenset({"done", "completed", "failed"})

# The resume nudge. "keep going" is the exact text that resumed the motivating
# repro (b78039cc, a human typed it); the agent re-enters its turn loop on any
# user message, so the proven text is the lazy-correct choice.
CONTINUE_MESSAGE = "keep going"
FROM_NAME = "fno-recovery"


# ---------------------------------------------------------------------------
# Predicate
# ---------------------------------------------------------------------------

def classify(
    state: str,
    updated_at: Optional[str],
    now: datetime,
    idle_threshold_s: int,
    promise_present: bool,
) -> str:
    """Decide what to do with one footnote-launched bg session.

    Returns one of :data:`NUDGE`, :data:`SKIP_NEEDS_INPUT`,
    :data:`SKIP_TERMINAL`, :data:`SKIP_DONE`, :data:`NOT_STALE`.

    Order matters: terminal / needs-input / promise are checked before
    staleness so a done-or-waiting session is never nudged regardless of how
    stale its ``state.json`` is. Only a non-terminal, non-waiting, no-promise
    session whose ``updatedAt`` is older than the threshold is a nudge target.
    A missing or unparseable ``updatedAt`` is treated conservatively as
    not-stale (we cannot prove idleness, so we do not nudge).
    """
    if state == "needs-input":
        return SKIP_NEEDS_INPUT
    if state in _TERMINAL:
        return SKIP_TERMINAL
    if promise_present:
        return SKIP_DONE
    age = _age_seconds(updated_at, now)
    if age is None or age < idle_threshold_s:
        return NOT_STALE
    return NUDGE


def _age_seconds(updated_at: Optional[str], now: datetime) -> Optional[float]:
    """Seconds since ``updated_at`` (ISO8601), or None if absent/unparseable."""
    if not updated_at:
        return None
    raw = updated_at.strip()
    if raw.endswith("Z"):
        raw = raw[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(raw)
    except (ValueError, TypeError):
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return (now - dt).total_seconds()


# ---------------------------------------------------------------------------
# Candidate enumeration (registry provenance ∩ live bg sessions)
# ---------------------------------------------------------------------------

@dataclass
class Candidate:
    """A footnote-launched bg session reachable over its messaging socket."""

    short_id: str
    sock_path: str
    jobs_dir: object  # pathlib.Path; opaque here so tests can pass a tmp_path


@dataclass
class _SnapshotView:
    """Minimal view of state.json the sweep needs (state + updatedAt).

    The production ``read_state_fn`` returns a ``StateSnapshot`` which carries
    these same two attributes, so it is a drop-in; tests construct this directly.
    """

    state: str
    updated_at: Optional[str]


def iter_candidates(registry_entries: Iterable, locate_fn: Callable) -> list[Candidate]:
    """Join footnote's claude registry rows with live bg sessions.

    ``locate_fn(short_id)`` is ``_claude_session_registry.locate_session``: it
    returns a locator with a live ``messaging_socket_path`` + ``jobs_dir`` only
    for a ``kind == "bg"`` session whose socket is non-null, and ``None``
    otherwise. A registry row that is not claude, has no ``claude_short_id``, or
    has no matching live session is dropped — so the result is exactly the set
    of footnote-launched, currently-reachable bg sessions.
    """
    out: list[Candidate] = []
    for entry in registry_entries:
        if getattr(entry, "provider", None) != "claude":
            continue
        short_id = getattr(entry, "claude_short_id", None)
        if not short_id:
            continue
        loc = locate_fn(short_id)
        if loc is None:
            continue
        out.append(Candidate(short_id=short_id, sock_path=loc.messaging_socket_path, jobs_dir=loc.jobs_dir))
    return out


# ---------------------------------------------------------------------------
# Sweep
# ---------------------------------------------------------------------------

def _capped_key(short_id: str) -> str:
    """Sentinel key marking "recovery_capped already announced for this id".

    Lives in the same flat counts dict; the ``capped:`` prefix is collision-free
    because a real short_id is 8 hex chars and never contains ``:``. Keeps the
    cap event firing once (on the transition) instead of every tick.
    """
    return f"capped:{short_id}"


def recovery_sweep(
    now: datetime,
    cfg,
    *,
    candidates: list[Candidate],
    counts: dict,
    emit: Callable[[str, dict], None],
    read_state_fn: Callable,
    read_promise_fn: Callable,
    liveness_fn: Callable[[str], bool],
    send_fn: Callable[[str, str, str], None],
) -> None:
    """Classify each candidate and act: nudge (capped), skip, or stay silent.

    Every decision that *matters* emits exactly one event:
      - ``recovery_nudge`` when a resume is injected,
      - ``recovery_skipped{reason}`` when an idle-stale session is deliberately
        spared (``needs-input``), unreachable, or the send failed,
      - ``recovery_capped`` once when a session hits the per-session nudge cap.
    A done / not-yet-stale session is silent (no event) so healthy ticks do not
    spam the log. All I/O is injected so this is unit-testable offline.
    """
    for c in candidates:
        snap = read_state_fn(c.jobs_dir)
        promise = read_promise_fn(c.jobs_dir)
        decision = classify(snap.state, snap.updated_at, now, cfg.idle_threshold_seconds, promise)

        if decision == SKIP_NEEDS_INPUT:
            emit("recovery_skipped", {"short_id": c.short_id, "reason": "needs-input"})
            continue
        if decision != NUDGE:
            # SKIP_TERMINAL / SKIP_DONE / NOT_STALE: nothing to say.
            continue

        n = counts.get(c.short_id, 0)
        if n >= cfg.max_nudges:
            ck = _capped_key(c.short_id)
            if not counts.get(ck):
                counts[ck] = True
                emit("recovery_capped", {"short_id": c.short_id, "nudges": n})
            continue

        if not liveness_fn(c.sock_path):
            emit("recovery_skipped", {"short_id": c.short_id, "reason": "socket-unreachable"})
            continue

        try:
            send_fn(c.sock_path, CONTINUE_MESSAGE, FROM_NAME)
        except (ProviderSocketError, OSError) as exc:
            emit("recovery_skipped", {"short_id": c.short_id, "reason": "send-failed", "error": str(exc)})
            continue

        counts[c.short_id] = n + 1
        emit("recovery_nudge", {"short_id": c.short_id, "nudge_count": n + 1})


# ---------------------------------------------------------------------------
# Real I/O seams + the high-level entry the pr_watch tick calls
# ---------------------------------------------------------------------------

def read_promise(jobs_dir) -> bool:
    """True if ``timeline.jsonl`` contains a ``<promise>`` (session is done).

    Reads only the tail (last 64KB) — a ``<promise>`` is emitted at the end of
    the run, and the tail keeps the scan O(1) regardless of timeline length.
    Any read error is treated as "no promise" (conservative; a missing promise
    only risks an extra nudge, which the cap bounds).
    """
    timeline = Path(jobs_dir) / "timeline.jsonl"
    try:
        size = timeline.stat().st_size
        with open(timeline, "rb") as fh:
            if size > 65536:
                fh.seek(size - 65536)
            return b"<promise>" in fh.read()
    except OSError:
        return False


def _counts_path() -> Path:
    from fno import paths

    return paths.state_dir() / "recovery-nudges.json"


def load_counts() -> dict:
    """Load the per-session nudge counter (empty on any read/parse failure)."""
    try:
        return json.loads(_counts_path().read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}


def save_counts(counts: dict) -> None:
    """Persist the nudge counter; non-fatal on failure (best-effort state).

    ponytail: best-effort means the cap is *soft* under a persistent write
    failure — a failed save lets the next tick reload the pre-increment count
    and nudge again. Acceptable: the alternative (crashing the tick on a disk
    error) is worse, and a chronically unwritable ~/.fno is a louder problem.
    """
    try:
        path = _counts_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(counts), encoding="utf-8")
    except OSError:
        pass


def _safe_read_state(jobs_dir):
    """Read state.json, degrading a read error to a conservative empty view.

    An unreadable/mid-write state.json yields ``("", None)`` which classify
    treats as not-stale, so a transient read failure never causes a spurious
    nudge; the next tick retries.
    """
    from fno.agents.providers._claude_session_registry import read_state_json

    try:
        snap = read_state_json(Path(jobs_dir))
        return _SnapshotView(snap.state, snap.updated_at)
    except (OSError, json.JSONDecodeError, AttributeError, TypeError):
        # AttributeError/TypeError: a valid-JSON-but-non-object state.json (a
        # bare string/list) makes the parser's ``.get(...)`` raise. Degrade to
        # an empty view so ONE malformed session never aborts the sweep for the
        # rest of this tick; the next tick retries.
        return _SnapshotView("", None)


def _prune_keep(key: str, live: set) -> bool:
    """Keep a counts entry only while its session is still a live candidate."""
    if key.startswith("capped:"):
        return key[len("capped:"):] in live
    return key in live


def run_recovery_sweep(
    cfg,
    *,
    emit: Callable[[str, dict], None],
    now: Optional[datetime] = None,
    registry_load: Optional[Callable] = None,
    locate_fn: Optional[Callable] = None,
    read_state_fn: Optional[Callable] = None,
    read_promise_fn: Optional[Callable] = None,
    liveness_fn: Optional[Callable] = None,
    send_fn: Optional[Callable] = None,
    load_counts_fn: Optional[Callable] = None,
    save_counts_fn: Optional[Callable] = None,
) -> int:
    """Build the real seams, run one sweep, persist counts. Returns candidate count.

    This is the single entry the ``pr_watch`` tick calls. Every seam defaults to
    the real implementation but is injectable so the wiring is testable offline.
    The persisted counts are pruned to currently-live candidates each run so the
    file stays bounded and a long-gone session does not pin a stale count.
    """
    now = now or datetime.now(timezone.utc)

    if registry_load is None:
        from fno.agents.registry import load_registry

        registry_load = load_registry
    if locate_fn is None:
        from fno.agents.providers._claude_session_registry import locate_session

        locate_fn = locate_session
    if liveness_fn is None:
        from fno.agents.providers.claude import liveness_probe

        liveness_fn = liveness_probe
    if send_fn is None:
        from fno.agents.providers.claude import send_to_session

        send_fn = send_to_session
    read_state_fn = read_state_fn or _safe_read_state
    read_promise_fn = read_promise_fn or read_promise
    load_counts_fn = load_counts_fn or load_counts
    save_counts_fn = save_counts_fn or save_counts

    candidates = iter_candidates(registry_load(), locate_fn)
    counts = load_counts_fn()
    live = {c.short_id for c in candidates}
    counts = {k: v for k, v in counts.items() if _prune_keep(k, live)}

    recovery_sweep(
        now, cfg,
        candidates=candidates, counts=counts, emit=emit,
        read_state_fn=read_state_fn, read_promise_fn=read_promise_fn,
        liveness_fn=liveness_fn, send_fn=send_fn,
    )

    save_counts_fn(counts)
    return len(candidates)
