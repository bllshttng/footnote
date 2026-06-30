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
) -> str:
    """Decide what to do with one footnote-launched bg session.

    Returns one of :data:`NUDGE`, :data:`SKIP_NEEDS_INPUT`,
    :data:`SKIP_TERMINAL`, :data:`NOT_STALE`.

    Order matters: terminal / needs-input are checked before staleness so a
    done-or-waiting session is never nudged regardless of how stale its
    ``state.json`` is. Only a non-terminal, non-waiting session whose
    ``updatedAt`` is older than the threshold is a nudge target. A missing or
    unparseable ``updatedAt`` is treated conservatively as not-stale (we cannot
    prove idleness, so we do not nudge).

    "Done" is read from the CC job ``state`` (``done``/``completed``/``failed``),
    NOT from a ``<promise>`` in the transcript: a target ``<promise>`` is only
    the model's *claim* of completion. loop-check's done-reads can reject it and
    the session keeps going, so a past promise does not mean the work shipped —
    a session that emitted ``<promise>`` and was then blocked is still working
    and must remain recoverable. The terminal job state is the real authority,
    and it is reached only when the session actually exited cleanly.
    """
    if state == "needs-input":
        return SKIP_NEEDS_INPUT
    if state in _TERMINAL:
        return SKIP_TERMINAL
    age = _age_seconds(updated_at, now)
    if age is None or age < idle_threshold_s:
        return NOT_STALE
    return NUDGE


def _age_seconds(updated_at: Optional[str], now: datetime) -> Optional[float]:
    """Seconds since ``updated_at`` (ISO8601), or None if absent/unparseable.

    Both operands are normalized to timezone-aware UTC before subtracting so a
    naive ``now`` (or a naive ``updatedAt``) never raises a mixed-aware/naive
    ``TypeError``.
    """
    if not isinstance(updated_at, str) or not updated_at.strip():
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
    if now.tzinfo is None:
        now = now.replace(tzinfo=timezone.utc)
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
    # cwd + name come from the registry row. They are unused on the nudge path
    # and only carried for the failover re-dispatch (x-7abe): the worktree cwd
    # holds the node's target-state.md, and the name is the handle for
    # ``fno agents stop``. Both default None so nudge-path callers/tests need not
    # supply them.
    cwd: Optional[str] = None
    name: Optional[str] = None


@dataclass
class _SnapshotView:
    """Minimal view of state.json the sweep needs (state + updatedAt).

    The production ``read_state_fn`` returns a ``StateSnapshot`` which carries
    these same attributes, so it is a drop-in; tests construct this directly.
    """

    state: str
    updated_at: Optional[str]
    # The dead session's last ``output.result`` text. For an out-of-usage death
    # it carries the provider error ("API Error: ... rate limit ...") that the
    # failover branch classifies (x-7abe). None on the nudge path / older tests.
    output_result: Optional[str] = None


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
        sock = getattr(loc, "messaging_socket_path", None)
        jobs_dir = getattr(loc, "jobs_dir", None)
        if not sock or jobs_dir is None:
            # locate_session's contract guarantees both, but a future locator
            # variant might not; skip rather than build an unusable candidate.
            continue
        out.append(Candidate(
            short_id=short_id, sock_path=sock, jobs_dir=jobs_dir,
            cwd=getattr(entry, "cwd", None), name=getattr(entry, "name", None),
        ))
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
    liveness_fn: Callable[[str], bool],
    send_fn: Callable[[str, str, str], None],
    failover_fn: Optional[Callable[["Candidate", object], str]] = None,
) -> None:
    """Classify each candidate and act: failover, nudge (capped), skip, or stay silent.

    Every decision that *matters* emits exactly one event:
      - ``failover_swapped`` when an out-of-usage session rotates providers,
      - ``failover_blocked{reason}`` when a swap is wanted but bounded (thrash /
        queue-exhausted),
      - ``recovery_nudge`` when a resume is injected,
      - ``recovery_skipped{reason}`` when an idle-stale session is deliberately
        spared (``needs-input``), unreachable, or the send failed,
      - ``recovery_capped`` once when a session hits the per-session nudge cap.
    A done / not-yet-stale session is silent (no event) so healthy ticks do not
    spam the log. All I/O is injected so this is unit-testable offline.

    ``failover_fn`` (x-7abe) is the only way the failover branch activates: when
    None (every nudge-path caller / test), the sweep behaves exactly as the
    x-f47c watchdog. When supplied, a stale session whose last error is a
    *swap-class* one (rate-limit / quota / auth / 5xx) routes to provider
    failover INSTEAD of a nudge - nudging "keep going" at a rate-limited provider
    just re-hits the limit. A connection-drop (non-swap class) still nudges.
    """
    for c in candidates:
        snap = read_state_fn(c.jobs_dir)
        decision = classify(snap.state, snap.updated_at, now, cfg.idle_threshold_seconds)

        if decision == SKIP_NEEDS_INPUT:
            emit("recovery_skipped", {"short_id": c.short_id, "reason": "needs-input"})
            continue
        if decision != NUDGE:
            # SKIP_TERMINAL / NOT_STALE: nothing to say.
            continue

        # Out-of-usage failover (x-7abe): a swap-class death means the provider
        # is rate-limited/quota'd, so rotate + re-dispatch instead of nudging the
        # same dead provider. Checked before the nudge cap (failover has its own
        # per-phase storm-cap inside attempt_swap) and before liveness (a swap
        # re-dispatches a fresh session, it does not need the dead socket).
        if failover_fn is not None:
            err = classify_session_error(getattr(snap, "output_result", None))
            if err is not None and getattr(err, "triggers_swap", False):
                outcome = failover_fn(c, err)
                if outcome == "swapped":
                    emit("failover_swapped", {"short_id": c.short_id})
                    continue
                if outcome in ("blocked-thrash", "queue-exhausted"):
                    emit("failover_blocked", {"short_id": c.short_id, "reason": outcome})
                    continue
                # "no-swap": the controller declined (e.g. NO_SWAP_NEEDED); fall
                # through to the normal nudge defensively.

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

def _counts_path() -> Path:
    from fno import paths

    return paths.state_dir() / "recovery-nudges.json"


def load_counts() -> dict:
    """Load the per-session nudge counter (empty on any read/parse failure).

    ``UnicodeDecodeError`` (a corrupt non-UTF-8 file) is caught alongside
    OSError/JSONDecodeError so a damaged counter file degrades to "start fresh"
    rather than crashing the sweep.
    """
    try:
        data = json.loads(_counts_path().read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


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
        return _SnapshotView(snap.state, snap.updated_at, snap.output_result)
    except (OSError, json.JSONDecodeError, UnicodeDecodeError, AttributeError, TypeError):
        # AttributeError/TypeError: a valid-JSON-but-non-object state.json (a
        # bare string/list) makes the parser's ``.get(...)`` raise.
        # UnicodeDecodeError: a corrupt non-UTF-8 state.json. Either way, degrade
        # to an empty view so ONE malformed session never aborts the sweep for
        # the rest of this tick; the next tick retries.
        return _SnapshotView("", None)


# ---------------------------------------------------------------------------
# Out-of-usage provider failover (x-7abe)
#
# This wires the already-built+tested failover engine
# (``adapters/providers/error_taxonomy`` + ``failover.FailoverController``) into
# the recovery watchdog - the ONLY live autonomous loop in production. The design
# named an in-loop megawalk path as primary, but that path (``megawalk_drivers``,
# ``DriverWithFallback``, ``map_*_error``) has zero production call sites and
# footnote's real autonomous loop (Rust ``loop run`` + ``claude --bg`` +
# stop-hook) has no synchronous Python point that catches a 429. So the watchdog
# is the sole integration point and the in-loop "primary" is N/A. (cv-59ef0909)
# ---------------------------------------------------------------------------

def classify_session_error(output_result: Optional[str]):
    """Classify a dead session's last ``output.result`` into a ``NormalizedError``.

    Returns the ``NormalizedError`` (callers check ``.triggers_swap``) or None
    when there is no text to classify. Reuses the shipped ``normalize`` text
    rules - "rate limit" / "quota exceeded" -> swap-class; a clean connection-drop
    has none of those markers -> UNKNOWN (``triggers_swap`` False), so it still
    nudges. No new error logic.
    """
    if not output_result or not isinstance(output_result, str):
        return None
    from fno.adapters.providers.error_taxonomy import normalize

    return normalize(http_status=None, exit_code=None, body=output_result)


def _node_id_from_worktree(cwd: str) -> Optional[str]:
    """Read ``graph_node_id`` from the dead session's worktree target-state.md.

    The registry row carries the worktree cwd but not the node id; the immutable
    manifest there does. Best-effort: any read/parse miss returns None and the
    re-dispatch is skipped (the swap still happened).
    """
    try:
        text = (Path(cwd) / ".fno" / "target-state.md").read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("graph_node_id:"):
            val = s.split(":", 1)[1].strip().strip('"').strip("'")
            return val or None
    return None


def _redispatch(candidate: "Candidate", new_provider: str) -> None:
    """Best-effort: stop the rate-limited session and respawn ``/target`` on the
    swapped provider, continuing in the SAME worktree (work-so-far lives in the
    branch's atomic commits there).

    ponytail: best-effort respawn. The GUARANTEED win is the swap itself - the
    global provider pointer is rotated and the dead session stops getting
    pointless nudges. This respawn is gravy: if it fails (claim still held,
    manifest already initialized, spawn errors), the node stays claimed-but-idle
    and the next ``fno backlog advance`` / a human picks it up on the now-healthy
    provider. Upgrade path if respawn proves flaky in the wild: harden
    claim-reclaim + worktree continuity (the unverified-offline edges). Reuses the
    canonical ``fno agents spawn --substrate bg`` shape from dispatch-node.sh.
    """
    import subprocess

    cwd = getattr(candidate, "cwd", None)
    if not cwd:
        return
    node = _node_id_from_worktree(cwd)
    if not node:
        return
    name = getattr(candidate, "name", None)
    agent = f"failover-{candidate.short_id}"
    try:
        if name:
            # Release the dead session's node claim so the respawn can re-claim.
            subprocess.run(
                ["fno", "agents", "stop", name],
                cwd=cwd, capture_output=True, timeout=30, check=False,
            )
        # no-merge: an autonomous worker lands a PR for review, never auto-merges.
        subprocess.run(
            ["fno", "agents", "spawn", "--provider", new_provider,
             "--substrate", "bg", "--cwd", cwd, agent, f"/target no-merge {node}"],
            cwd=cwd, capture_output=True, timeout=60, check=False,
        )
    except (OSError, subprocess.SubprocessError):
        # Non-fatal: the swap already landed; never let a respawn miss crash the
        # sweep for the rest of this tick.
        pass


def _default_failover(candidate: "Candidate", error) -> str:
    """Real ``failover_fn``: rotate the active provider via the shipped controller.

    Returns ``"swapped"`` / ``"blocked-thrash"`` / ``"queue-exhausted"`` /
    ``"no-swap"``. ``phase_id`` is keyed on the dead session's short_id so the
    controller's per-phase storm-cap bounds how many times one stuck session
    rotates. Every failure mode degrades to ``"no-swap"`` (the caller then nudges
    defensively) rather than crashing the sweep.
    """
    from fno.adapters.providers.failover import FailoverController, SwapDecision
    from fno.adapters.providers.loader import read_active_provider_atomic
    from fno.adapters.providers.dispatch import _default_settings_path
    from fno import paths

    try:
        settings_path = _default_settings_path()
        active = read_active_provider_atomic(settings_path=settings_path).id
        state_path = paths.state_dir() / "failover-state.json"
        ctrl = FailoverController(
            settings_path=settings_path, state_path=state_path,
            phase_id=f"{candidate.short_id}:recovery",
        )
        result = ctrl.attempt_swap(current_provider_id=active, error=error)
    except Exception:  # noqa: BLE001 - failover must never break the sweep
        return "no-swap"

    if result.decision is SwapDecision.SWAPPED:
        _redispatch(candidate, result.new_provider_id)
        return "swapped"
    if result.decision is SwapDecision.BLOCKED_THRASH:
        return "blocked-thrash"
    if result.decision is SwapDecision.QUEUE_EXHAUSTED:
        return "queue-exhausted"
    return "no-swap"


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
    liveness_fn: Optional[Callable] = None,
    send_fn: Optional[Callable] = None,
    load_counts_fn: Optional[Callable] = None,
    save_counts_fn: Optional[Callable] = None,
    failover_fn: Optional[Callable] = None,
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
    load_counts_fn = load_counts_fn or load_counts
    save_counts_fn = save_counts_fn or save_counts
    # Default-on in production: the failover branch activates whenever a stale
    # session's last error is swap-class. With a single configured provider the
    # controller returns QUEUE_EXHAUSTED (a bounded stop), so wiring it on by
    # default is safe (x-7abe).
    failover_fn = failover_fn or _default_failover

    candidates = iter_candidates(registry_load(), locate_fn)
    counts = load_counts_fn()
    live = {c.short_id for c in candidates}
    counts = {k: v for k, v in counts.items() if _prune_keep(k, live)}

    recovery_sweep(
        now, cfg,
        candidates=candidates, counts=counts, emit=emit,
        read_state_fn=read_state_fn,
        liveness_fn=liveness_fn, send_fn=send_fn,
        failover_fn=failover_fn,
    )

    save_counts_fn(counts)
    return len(candidates)
