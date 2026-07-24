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
  agents registry (``provider == "claude"`` rows carry the jobId in ``short_id``);
  liveness + the messaging socket come from ``locate_session`` reading
  ``~/.claude/sessions``. The join is exactly "footnote-launched AND reachable",
  which satisfies the invariant *only ever touch sessions footnote launched*.
  Under-coverage (a footnote bg session missing from the registry) is the SAFE
  failure: we never nudge an arbitrary CC session, we just might miss one.

- **Transcript truth owns liveness.** ``session_truth`` supplies the content-aware
  activity state and age; frozen ``state.json`` remains phase metadata only.

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
import shlex
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterable, Optional

from fno import _subprocess_util
from fno.agents.providers.claude import ProviderSocketError

# The error the real send seam raises; aliased so callers/tests have one name.
_SendError = ProviderSocketError

# Decision constants returned by :func:`classify`.
NUDGE = "nudge"
SKIP_NEEDS_INPUT = "skip-needs-input"
SKIP_TERMINAL = "skip-terminal"
NOT_STALE = "not-stale"

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
    *,
    truth_state: str,
    truth_age_s: Optional[float],
    mission_complete: Optional[bool] = None,
) -> str:
    """Classify from family-1 transcript truth; state.json is phase metadata.

    ``mission_complete`` is the family-2 half (x-5583): family 1 calls any
    terminal ``<promise>`` done, so on its own it lets a worker that promised
    and then died abnormally suppress recovery AND failover forever. Only an
    explicit ``False`` - positive evidence of an unfinished mission - relaxes
    that; True and None (unverifiable) keep the terminal skip.
    """
    del updated_at, now
    if state == "needs-input" or truth_state == "your-move":
        return SKIP_NEEDS_INPUT
    if truth_state == "done":
        if mission_complete is not False:
            return SKIP_TERMINAL
        # Hollow promise: fall through to the staleness gate below, so a fresh
        # promise mid-finalize is left alone and only an idle one is nudged.
    if truth_state in {"unknown", "watching"}:
        return NOT_STALE
    if truth_state == "stalled":
        return NUDGE
    if truth_age_s is None or truth_age_s < idle_threshold_s:
        return NOT_STALE
    return NUDGE


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
    session_id: Optional[str] = None
    agent: str = "claude"


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
    otherwise. A registry row that is not claude, has no ``short_id``, or
    has no matching live session is dropped — so the result is exactly the set
    of footnote-launched, currently-reachable bg sessions.
    """
    out: list[Candidate] = []
    for entry in registry_entries:
        if getattr(entry, "harness", None) != "claude":
            continue
        short_id = getattr(entry, "short_id", "") or None
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
            session_id=(getattr(entry, "harness_session_id", None)
                        or getattr(entry, "cc_session_id", None) or short_id),
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
    truth_fn: Callable[[Candidate], dict],
    liveness_fn: Callable[[str], bool],
    send_fn: Callable[[str, str, str], None],
    failover_fn: Optional[Callable[["Candidate", object], str]] = None,
    mission_complete_fn: Optional[Callable[["Candidate"], Optional[bool]]] = None,
) -> None:
    """Classify each candidate and act: failover, nudge (capped), skip, or stay silent.

    Every decision that *matters* emits exactly one event:
      - ``failover_swapped`` when an out-of-usage session rotates providers,
      - ``failover_blocked{reason}`` when a swap is wanted but storm-capped
        (thrash). A queue-exhausted result (no alternate) is NOT blocked - it
        falls through to the bounded nudge below,
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

    At most ONE provider rotation fires per sweep tick (``rotated`` guard): a
    swap mutates the *global* active provider, so a second candidate evaluated
    after it would have its error mis-attributed to the already-swapped-to
    provider (codex P2). The remaining stale sessions nudge this tick and are
    reconsidered next tick against the settled provider.
    """
    rotated = False  # one provider rotation per tick (P2: global active mutation)
    for c in candidates:
        snap = read_state_fn(c.jobs_dir)
        truth = truth_fn(c)
        truth_state = str(truth.get("state") or "unknown")
        # Probe only behind a terminal promise - every other state is already
        # decided by family 1 alone, so this stays off the hot path.
        mc = (mission_complete_fn(c)
              if truth_state == "done" and mission_complete_fn is not None
              else None)
        decision = classify(
            snap.state, snap.updated_at, now, cfg.idle_threshold_seconds,
            truth_state=truth_state,
            truth_age_s=truth.get("last_activity_age_s"),
            mission_complete=mc,
        )

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
        if failover_fn is not None and not rotated:
            err = classify_session_error(getattr(snap, "output_result", None))
            if err is not None and getattr(err, "triggers_swap", False):
                outcome = failover_fn(c, err)
                if outcome in ("swapped", "rotated-no-worker", "notified"):
                    # Either way the global active provider rotated, so no
                    # further swap this tick.
                    rotated = True
                    # Honest event: redispatched=True only when a replacement
                    # worker actually started (codex P1 — a swallowed spawn
                    # failure must NOT report a phantom redispatch).
                    emit("failover_swapped", {
                        "short_id": c.short_id,
                        "redispatched": outcome == "swapped",
                    })
                    if outcome != "rotated-no-worker":
                        # "swapped" (worker/thread respawned) and "notified" (US4/US5:
                        # the human got the exact resume command for a session we
                        # could not auto-revive) are both terminal for this session -
                        # nudging a swapped-away/exhausted session just re-hits the
                        # dead provider, which is exactly what failover avoids.
                        continue
                    # "rotated-no-worker": the swap landed on a provider we
                    # cannot bg-redispatch a /target onto (non-claude) or the
                    # spawn failed; fall through to the bounded nudge so the
                    # session is not left dead-and-unnudged (codex P1).
                elif outcome == "blocked-thrash":
                    # Storm-cap reached: genuine churn, deliberate bounded stop.
                    emit("failover_blocked", {"short_id": c.short_id, "reason": outcome})
                    continue
                # "queue-exhausted" (no alternate provider exists — the common
                # single-provider case) and "no-swap" (controller declined): fall
                # through to the x-f47c nudge. With nothing to swap to, the
                # bounded nudge is the right fallback — the rate-limit window may
                # clear and the per-session cap stops it spinning, so this is
                # strictly no worse than the pre-failover watchdog.

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
    """Read optional phase/error metadata; transcript truth owns liveness."""
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
# named an in-loop megawalk path as primary, but that path had zero production
# call sites and footnote's real autonomous loop (Rust ``loop run`` +
# ``claude --bg`` + stop-hook) has no synchronous Python point that catches a
# 429. So the watchdog is the sole integration point. (cv-59ef0909)
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
    except (OSError, UnicodeDecodeError):
        # UnicodeDecodeError is a ValueError, not an OSError - a non-UTF-8
        # manifest must degrade to "no node" rather than crash the sweep.
        return None
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("graph_node_id:"):
            val = s.split(":", 1)[1].strip().strip('"').strip("'")
            return val or None
    return None


def _worktree_is_node_less(cwd: str) -> bool:
    """True ONLY when we can POSITIVELY confirm the worktree runs no ``/target``
    session - a genuine node-less bg thread.

    ``_node_id_from_worktree`` returns None for two different things (no node in
    the manifest AND an unreadable manifest), so it cannot gate revival: a
    node-bound worker whose ``.fno`` symlink transiently breaks mid-repack would
    read as node-less and be misrouted into transcript revival (which lacks the
    claim handling ``_redispatch`` does). Gate on manifest ABSENCE instead: the
    ``.fno`` dir must be reachable AND carry no ``target-state.md``. Any ambiguity
    (unreachable ``.fno`` symlink, an existing-but-unreadable manifest, a stat
    error) returns False, so a transient failure falls through to the node-bound
    path and its bounded nudge rather than a wrong revival."""
    try:
        fno_dir = Path(cwd) / ".fno"
        if not fno_dir.is_dir():
            # No .fno, OR a transiently-broken .fno symlink: cannot confirm
            # node-less-ness, so treat as node-bound (conservative).
            return False
        return not (fno_dir / "target-state.md").exists()
    except OSError:
        return False


def _node_is_done(node: str) -> bool:
    """True iff ``node`` resolves in the graph and is already ``done``.

    Guards the respawn against a node that raced to completion (x-370f AC1-EDGE):
    re-dispatching a finished node would spawn a worker with nothing to do. Any
    load miss (absent / corrupt graph, unknown id) degrades to False so the
    respawn proceeds — the claim + spawn path is the real backstop, not this read.
    """
    try:
        from fno.graph.load import load_graph

        for entry in load_graph():
            if entry.get("id") == node:
                return entry.get("status") == "done"
    except Exception:  # noqa: BLE001 - a status read must never crash the sweep
        return False
    return False


def mission_complete(candidate: "Candidate") -> Optional[bool]:
    """Did this worker's MISSION finish, per the node's external artifacts?

    Family 1 (transcript truth) answers "is the stream terminal"; a ``<promise>``
    makes it ``done`` whether or not anything shipped. This is the family-2 half
    the recovery gate needs: the graph artifact that only a real completion
    leaves behind. Claim state deliberately plays no part - claims are
    PID-anchored, so a finished worker and an abandoned one both read stale.

    Returns None (unverifiable) for an unresolvable mission or any read failure;
    the caller treats None as "keep today's suppression" (fail closed).
    """
    try:
        from fno.agents.truth_status import parse_worker_mission

        parsed = parse_worker_mission(candidate.name)
        # Manifest first: the runtime wrote it, so unlike a worker name (a mere
        # convention - `tgt-x-4175-liveness` ships) it cannot drift. The one
        # exception is a positively think-named worker: it writes no manifest of
        # its own but spawns with --cwd on the node's canonical root, where an
        # unrelated /target session's manifest can sit. Reading that would
        # answer about the wrong node, so its name is the only signal that is
        # about THIS worker.
        manifest_node = (
            _node_id_from_worktree(candidate.cwd)
            if candidate.cwd and (parsed is None or parsed[1] != "think")
            else None
        )
        if manifest_node:
            node_id, kind = manifest_node, "target"
        elif parsed is not None:
            node_id, kind = parsed
        else:
            return None

        from fno.graph.load import load_graph

        entry = next((e for e in load_graph() if e.get("id") == node_id), None)
        if entry is None:
            return None
        if entry.get("status") == "done":
            return True
        if kind == "think":
            # spawn_think's contract: a design pass with no linked plan_path failed.
            return bool(str(entry.get("plan_path") or "").strip())
        # A PR existing is the completion floor - red CI and pending bots are
        # pr_watch's and /fno:pr check's beat, not the watchdog's (LD 4).
        return bool(entry.get("pr_number") or entry.get("pr_url"))
    except Exception:  # noqa: BLE001 - a probe must never crash the sweep
        return None


def _release_lane_slot(node: str, cwd: str) -> None:
    """Free a dead lane's parallel-lane slot (parallel mode x-42d5, G4).

    Called only when the respawn did NOT start: a successful respawn keeps the
    slot for the new worker's target-init reconcile, and a post-init slot is
    pid-anchored so it frees itself on worker death anyway. What this shortens
    is the dispatch-time (pre-init, TTL-anchored) slot's linger - while it is
    live, ``select_lane_fill`` skips the node as "owned by a peer lane", so
    without the release a failed respawn would leave the node unselectable for
    up to the slot TTL. Best-effort, mirroring the force-release shell-out.
    """
    import logging
    import subprocess

    log = logging.getLogger(__name__)
    try:
        rel = subprocess.run(
            [*_subprocess_util.fno_py_cmd(), "claim", "lane-release", "--lane-id", node],
            cwd=cwd, capture_output=True, timeout=30, check=False,
        )
        if rel.returncode != 0:
            log.warning(
                "recovery: lane-release failed for %s (exit %s); slot lingers to TTL",
                node, rel.returncode,
            )
    except (OSError, subprocess.SubprocessError) as exc:
        log.warning("recovery: lane-release failed for %s: %s", node, exc)


def _redispatch(candidate: "Candidate", *, pre_spawn: Optional[Callable[[], bool]] = None) -> bool:
    """Stop the rate-limited session and respawn ``/target`` on the now-active
    (swapped) provider, continuing in the SAME worktree (work-so-far lives in the
    branch's atomic commits there). Returns True iff a replacement worker was
    actually launched (spawn exit 0).

    Caller guarantees the new active provider's cli is ``claude`` before calling,
    so the substrate is ``bg`` (claude-only) and ``--provider claude`` selects
    the now-active claude record the swap installed in settings.yaml. (A
    non-claude swap cannot bg-redispatch a multi-phase /target, so the caller
    skips this entirely — codex P1.)

    Ordered reuse-first sequence (x-370f residual 1): stop the worker, then
    ``fno claim force-release`` the node claim (the verified reliability gap —
    ``stop`` alone does NOT free the claim, per the auto-continue-wedge finding,
    so without this the respawn's ``target init`` refuses on the held claim and
    the node goes claimed-but-idle), then spawn via the canonical shape. A
    non-zero force-release means the claim is still held → skip the spawn (it
    would refuse anyway) and let the caller nudge. Any miss returns False and the
    caller falls back to the bounded nudge, so the session is never left
    dead-and-unnudged. Reuses the canonical ``fno agents spawn --substrate bg``
    shape from dispatch-node.sh.
    """
    import subprocess

    cwd = getattr(candidate, "cwd", None)
    if not cwd:
        return False
    node = _node_id_from_worktree(cwd)
    if not node:
        return False
    if _node_is_done(node):
        # Raced to completion: nothing to continue, so do not re-dispatch.
        return False
    name = getattr(candidate, "name", None)
    agent = f"failover-{candidate.short_id}"
    try:
        if name:
            # Kill the rate-limited worker. This does NOT free its node claim.
            stopped = subprocess.run(
                [*_subprocess_util.fno_py_cmd(), "agents", "stop", name],
                cwd=cwd, capture_output=True, timeout=30, check=False,
            )
            if stopped.returncode != 0:
                # Stop failed → the worker may still be live. force-releasing its
                # claim (an admin override that drops it regardless of holder) and
                # then spawning would put two /target workers on one node. Bail to
                # the nudge to preserve the at-most-one-worker invariant (codex P2).
                return False
        # Free the dead session's node claim so the respawn can re-claim it.
        # force-release is idempotent (a claim already self-released by a late
        # worker is success), so this also covers the stop/self-release race.
        rel = subprocess.run(
            [*_subprocess_util.fno_py_cmd(), "claim", "force-release", f"node:{node}",
             "-R", f"failover respawn {candidate.short_id}"],
            cwd=cwd, capture_output=True, timeout=30, check=False,
        )
        if rel.returncode != 0:
            # Claim still held → a spawn would refuse on it. Bail so the caller
            # nudges instead of reporting a respawn that cannot start.
            return False
        # US3 managed auto-switch: materialize the swapped-to account into the
        # shared slot HERE - after the exhausted worker is stopped (so it no
        # longer pins the slot; the live-pin gate would otherwise defer) and its
        # claim is freed, but BEFORE the replacement spawns (which must read the
        # NEW account's creds). A False result (disarmed / live-pin defer by
        # ANOTHER live session / store failure) aborts the respawn: free the lane
        # slot and bail to the nudge, same as a spawn failure.
        if pre_spawn is not None and not pre_spawn():
            _release_lane_slot(node, cwd)
            return False
        # --provider claude: the swap already installed the new claude record as
        # active in settings.yaml, so the kind is what spawn needs. no-merge: an
        # autonomous worker lands a PR for review, never auto-merges.
        proc = subprocess.run(
            [*_subprocess_util.fno_py_cmd(), "agents", "spawn", "--harness", "claude",
             "--substrate", "bg", "--cwd", cwd, "--name", agent,
             f"/target no-merge {node}"],
            cwd=cwd, capture_output=True, timeout=60, check=False,
        )
        if proc.returncode != 0:
            # No replacement worker started: the node claim is already freed
            # (above), so also free any dispatch-time lane slot or lane-fill
            # keeps skipping the node as peer-owned until the slot TTL (G4).
            _release_lane_slot(node, cwd)
            return False
        return True
    except (OSError, subprocess.SubprocessError):
        # Non-fatal: the swap already landed; never let a respawn miss crash the
        # sweep for the rest of this tick.
        return False


def _auto_switch_enabled(repo_root: Optional[str] = None) -> bool:
    """``config.providers.auto_switch`` (default False), read from the dead
    session's project-local config when its worktree (``repo_root``) is known.

    Fail-safe: any read miss returns False, so an unreadable config never arms the
    shared-slot mutation. The caller checks this BEFORE stopping the exhausted
    worker, so a disarmed managed swap leaves the worker alive for the bounded
    nudge rather than stopping it for a switch that will not happen.
    """
    try:
        from fno.adapters.providers.loader import load_providers

        rr = Path(repo_root) if repo_root else None
        return bool(getattr(load_providers(repo_root=rr), "auto_switch", False))
    except Exception:  # noqa: BLE001 - config read must never crash the sweep
        return False


def _materialize_managed_switch(record_id: str, repo_root: Optional[str] = None) -> bool:
    """Materialize a managed account's credentials into the shared slot after an
    auto-switch swap landed on it (US3). Returns True iff the slot now holds the
    new account's verified credentials, so the redispatch reads live creds.

    Runs as ``_redispatch``'s ``pre_spawn`` hook, i.e. AFTER the exhausted worker
    is stopped (so it no longer pins the slot) and BEFORE the replacement spawns.
    Loads the dead session's project-local config (``repo_root`` = its worktree)
    so a worktree-local ``auto_switch`` / record set is honored. Re-checks
    ``auto_switch`` as defense-in-depth (the caller already gated it). A live-pin
    defer by ANOTHER live session (``SwitchDeferred``) or any store/keychain
    failure returns False - never redispatch onto un-switched (exhausted) creds.
    Reuses ``managed.switch`` (capture-before-overwrite + live-pin gate) and its
    ``account_switched`` emit from the manual `use` path (US2); no new switch
    logic here.
    """
    try:
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers import managed
        from fno.agents.events import emit as _emit

        config = load_providers(repo_root=Path(repo_root) if repo_root else None)
        if not getattr(config, "auto_switch", False):
            return False
        record = config.by_id.get(record_id)
        if record is None or record.auth != "managed":
            return False
        managed.switch(record, by_id=config.by_id, emit_fn=_emit, pin_policy="defer")
        return True
    except Exception:  # noqa: BLE001 - a materialize miss must never crash the sweep; degrade to nudge
        return False


# ---------------------------------------------------------------------------
# US4/US5: node-less bg-thread revival + interactive notify
#
# A failover swap rotates the active account, but a node-LESS bg thread (a
# footnote-launched ``claude --bg`` session whose cwd has no target-state.md - an
# ask/relay worker, a bare bg thread) has no node for ``_redispatch`` to /target.
# Revival maps the session kind to an action (epic x-e4a7 revival table):
#   - transcript visible to the new account -> respawn ``claude --bg --resume
#     <uuid> "keep going"`` under the new env (US4).
#   - transcript NOT visible (unshared two-dir) -> notify the human with the exact
#     copy-paste resume command (US5 / AC3-FR); never resume a missing transcript.
# An interactive session never reaches the sweep (``locate_session`` filters
# ``kind == "bg"``), so the same notify helper is the "footnote cannot revive it"
# path for it too.
# ---------------------------------------------------------------------------


def _resolve_session_uuid(short_id: str) -> Optional[str]:
    """Full session UUID for a bg ``short_id`` (the ``--resume`` key), or None.

    Best-effort: a registry-read miss degrades to None so revival falls back to the
    bounded nudge rather than crashing the sweep."""
    try:
        from fno.agents.providers._claude_session_registry import resolve_session_uuid

        return resolve_session_uuid(short_id)
    except Exception:  # noqa: BLE001 - a resolve miss must never crash the sweep
        return None


def _target_projects_dir(provider_id: str, repo_root: Optional[str]) -> Path:
    """The ``projects/`` dir the NEW account reads transcripts from.

    ``dispatch_env`` gives the account's ``CLAUDE_CONFIG_DIR`` (managed shares the
    default ``~/.claude``; oauth_dir points at its own dir). Any lookup miss
    degrades to ``~/.claude``. A wrong guess here only mis-decides visibility; the
    real backstop against a resume onto a missing transcript is the respawn itself,
    which spawns under the same account env and fails (falling to notify) when that
    env cannot reach the transcript."""
    base = Path.home() / ".claude"
    try:
        from fno.adapters.providers.dispatch import dispatch_env

        env = dispatch_env(provider_id, repo_root=Path(repo_root) if repo_root else None)
        override = env.get("CLAUDE_CONFIG_DIR")
        if override:
            base = Path(override)
    except Exception:  # noqa: BLE001 - degrade to the default slot
        pass
    return base / "projects"


def _transcript_visible(session_uuid: str, projects_dir: Path) -> bool:
    """Is ``<session_uuid>.jsonl`` present under ``projects_dir``? (AC3/AC4-FR)."""
    try:
        from fno.relay.registry import transcript_path_for

        return transcript_path_for(session_uuid, projects_dir=projects_dir) is not None
    except Exception:  # noqa: BLE001 - unreadable -> treat as not visible (notify)
        return False


def _resume_command(provider_id: str, repo_root: Optional[str], session_uuid: str) -> str:
    """The exact copy-paste resume command for the notify path (US5).

    Prefixes the new account's env (``CLAUDE_CONFIG_DIR=<dir> ``) when the record
    needs one; a managed record shares the default slot, so the prefix is empty."""
    prefix = ""
    try:
        from fno.adapters.providers.dispatch import dispatch_env

        env = dispatch_env(provider_id, repo_root=Path(repo_root) if repo_root else None)
        cfg = env.get("CLAUDE_CONFIG_DIR")
        if cfg:
            # shell-quote: a config dir with spaces (e.g. macOS "Application
            # Support") must stay one token when the human pastes the command.
            prefix = f"CLAUDE_CONFIG_DIR={shlex.quote(cfg)} "
    except Exception:  # noqa: BLE001 - best-effort prefix; bare command still resumes
        pass
    return f"{prefix}claude --resume {session_uuid}"


def _notify_manual_resume(candidate: "Candidate", snap, session_uuid: str) -> None:
    """US5: one OS notification carrying the exact resume command for a session
    footnote could not auto-revive (unshared transcript / interactive). Best-effort."""
    try:
        from fno.notify._impl import send_notification

        cmd = _resume_command(snap.id, getattr(candidate, "cwd", None), session_uuid)
        send_notification(
            f"footnote: switched to {snap.id}",
            f"Session {candidate.short_id} needs a manual resume:\n{cmd}",
        )
    except Exception:  # noqa: BLE001 - a notify miss must never crash the sweep
        pass


def _respawn_bg_resume(
    candidate: "Candidate",
    session_uuid: str,
    *,
    pre_spawn: Optional[Callable[[], bool]] = None,
) -> bool:
    """Stop the dead node-less thread and respawn a bg supervisor RESUMING its
    transcript under the now-active account, seeding the continue turn. Returns
    True iff the replacement spawned.

    Mirrors ``_redispatch``'s stop -> pre_spawn -> spawn ordering, minus the claim
    handling (a node-less thread holds no node claim). ``pre_spawn`` runs the
    managed materialize between the stop (which unpins the shared slot) and the
    spawn (which must read the new creds); a False result aborts the respawn."""
    import subprocess

    cwd = getattr(candidate, "cwd", None)
    name = getattr(candidate, "name", None)
    agent = f"revive-{candidate.short_id}"
    if not name:
        # No name to stop the dead thread by. The node-less path has no claim +
        # `target init` backstop against a double (unlike _redispatch), so a blind
        # --resume spawn could put two live supervisors on one transcript. Bail so
        # the caller notifies instead of risking the double.
        return False
    try:
        stopped = subprocess.run(
            [*_subprocess_util.fno_py_cmd(), "agents", "stop", name],
            cwd=cwd, capture_output=True, timeout=30, check=False,
        )
        if stopped.returncode != 0:
            # The thread may still be live; a second --resume supervisor would
            # double it. Bail so the caller notifies instead.
            return False
        if pre_spawn is not None and not pre_spawn():
            return False
        argv = [*_subprocess_util.fno_py_cmd(), "agents", "spawn", "--harness", "claude",
                "--substrate", "bg", "--resume", session_uuid]
        if cwd:
            argv += ["--cwd", cwd]
        argv += ["--name", agent, CONTINUE_MESSAGE]
        proc = subprocess.run(argv, cwd=cwd, capture_output=True, timeout=60, check=False)
        return proc.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def _revive_bg_thread(
    candidate: "Candidate", snap, repo_root: Optional[str], *, managed: bool
) -> str:
    """US4: revive a node-less bg thread after an auto-switch swap. Returns:
      - ``"swapped"``           resumed under the new account (a replacement started),
      - ``"notified"``          couldn't resume (unshared transcript / spawn miss) ->
                                OS-notified the human with the exact resume command,
      - ``"rotated-no-worker"`` nothing actionable (no uuid / disarmed) -> nudge."""
    uuid = _resolve_session_uuid(candidate.short_id)
    if not uuid:
        # No resolvable session id: cannot resume OR build a resume command; leave
        # it to the bounded nudge (same fallback a node-bound respawn miss uses).
        return "rotated-no-worker"
    if managed and not _auto_switch_enabled(repo_root):
        # Disarmed managed swap: the slot is never materialized, so a resume would
        # land on the exhausted account. Behave like the node-bound disarmed path.
        return "rotated-no-worker"
    projects_dir = _target_projects_dir(snap.id, repo_root)
    if not _transcript_visible(uuid, projects_dir):
        # AC3-FR: never resume against a transcript the new account cannot see.
        _notify_manual_resume(candidate, snap, uuid)
        return "notified"
    pre_spawn = (lambda: _materialize_managed_switch(snap.id, repo_root)) if managed else None
    if _respawn_bg_resume(candidate, uuid, pre_spawn=pre_spawn):
        return "swapped"
    # A stop / materialize / spawn miss after the transcript was visible: don't
    # nudge the exhausted thread; hand the human the resume command instead.
    _notify_manual_resume(candidate, snap, uuid)
    return "notified"


def _default_failover(candidate: "Candidate", error) -> str:
    """Real ``failover_fn``: rotate the active provider via the shipped controller.

    Returns one of:
      - ``"swapped"``           rotated AND a replacement worker started,
      - ``"rotated-no-worker"`` rotated but no worker (swapped onto a non-claude
                                provider /target cannot bg-run, or the spawn
                                failed) — the caller nudges as a fallback,
      - ``"blocked-thrash"`` / ``"queue-exhausted"`` / ``"no-swap"``.

    ``phase_id`` is keyed on the dead session's short_id so the controller's
    per-phase storm-cap bounds how many times one stuck session rotates. Every
    failure mode degrades to ``"no-swap"`` (the caller then nudges defensively)
    rather than crashing the sweep.
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
        # The swap installed result.new_provider_id as the active record. Re-read
        # to get its cli KIND + auth strategy (codex P1: new_provider_id is a
        # record id like "claude-secondary", NOT the "claude"/"codex" kind spawn
        # wants).
        try:
            snap = read_active_provider_atomic(settings_path=settings_path)
        except Exception:  # noqa: BLE001
            return "rotated-no-worker"
        # Autonomous /target only bg-runs on claude; a non-claude target cannot
        # be bg-redispatched (the Rust client rejects --substrate bg for it).
        if snap.cli != "claude":
            return "rotated-no-worker"
        repo_root = getattr(candidate, "cwd", None)
        managed = getattr(snap, "auth", None) == "managed"
        # US4: a node-less bg thread (a live worktree with NO target-state
        # manifest) has no /target to redispatch; resume its transcript under the
        # new account instead, or notify when it is not visible there. Gate on
        # confirmed manifest absence (not "no node id"): a missing cwd, an
        # unreadable/transiently-unreachable manifest, and a real node-bound worker
        # all stay on the node-bound path below (its _redispatch handles the miss
        # -> nudge), so a transient .fno read failure never misroutes a worker into
        # revival.
        if repo_root and _worktree_is_node_less(repo_root):
            return _revive_bg_thread(candidate, snap, repo_root, managed=managed)
        # A managed record shares ONE credential slot, so the swap only flipped
        # the routing pointer - the slot still holds the exhausted account's
        # creds. The replacement must read the NEW account's creds, so the slot is
        # materialized as _redispatch's pre_spawn step (AFTER the exhausted worker
        # is stopped, so it no longer pins the slot; BEFORE the replacement
        # spawns). Gate on auto_switch BEFORE stopping: a disarmed managed swap
        # leaves the worker alive for the bounded nudge rather than stopping it
        # for a switch that will not happen. oauth_dir/api_key records need no
        # materialization (env-var switch at spawn), so they redispatch as before.
        if managed:
            if not _auto_switch_enabled(repo_root):
                return "rotated-no-worker"
            if _redispatch(candidate,
                           pre_spawn=lambda: _materialize_managed_switch(snap.id, repo_root)):
                return "swapped"
            return "rotated-no-worker"
        if _redispatch(candidate):
            return "swapped"
        return "rotated-no-worker"
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
    truth_fn: Optional[Callable[[Candidate], dict]] = None,
    liveness_fn: Optional[Callable] = None,
    send_fn: Optional[Callable] = None,
    load_counts_fn: Optional[Callable] = None,
    save_counts_fn: Optional[Callable] = None,
    failover_fn: Optional[Callable] = None,
    mission_complete_fn: Optional[Callable] = None,
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
    if truth_fn is None:
        from fno.agents.session_truth import resolve_session_truth

        def truth_fn(candidate: Candidate) -> dict:
            return resolve_session_truth(
                candidate.name or candidate.short_id,
                resolve=lambda _handle: (candidate, []),
            )
    load_counts_fn = load_counts_fn or load_counts
    save_counts_fn = save_counts_fn or save_counts
    # Default-on in production: the failover branch activates whenever a stale
    # session's last error is swap-class. With a single configured provider the
    # controller returns QUEUE_EXHAUSTED (a bounded stop), so wiring it on by
    # default is safe (x-7abe).
    failover_fn = failover_fn or _default_failover
    mission_complete_fn = mission_complete_fn or mission_complete

    candidates = iter_candidates(registry_load(), locate_fn)
    counts = load_counts_fn()
    live = {c.short_id for c in candidates}
    counts = {k: v for k, v in counts.items() if _prune_keep(k, live)}

    recovery_sweep(
        now, cfg,
        candidates=candidates, counts=counts, emit=emit,
        read_state_fn=read_state_fn,
        truth_fn=truth_fn,
        liveness_fn=liveness_fn, send_fn=send_fn,
        failover_fn=failover_fn,
        mission_complete_fn=mission_complete_fn,
    )

    save_counts_fn(counts)
    return len(candidates)
