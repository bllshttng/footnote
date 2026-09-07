"""fno.agents.watchdog - the external fleet watchdog (x-55c3).

Runs OUTSIDE every session (a manual verb, or a leg on the pr_watch tick) and
decides, per fleet row: wake it, reroute it, or leave it. The decision reads
TRANSCRIPT truth keyed by session id; the fno registry and
``claude agents --json`` are hints (measured 2026-08-15: 8 roster rows claimed
``working`` on silent transcripts, and the claude view inverted live/dead; the
transcript was right every time). Row retirement is the daemon sweep's
question (x-c672), not this module's.

The classifier is one pure function over injected inputs, so tests need no
live fleet. Mechanisms delegate: the wake lane calls ``fno agents resume``
(x-c136) then confirms the message by CONTENT in the recipient transcript;
reroute reuses ``fno.recovery._redispatch`` (stop FIRST, then respawn).

Traps a stranger inherits (measured 2026-08-15, pinned by tests): node
identity joins on the recorded ``node:<id>`` claim holder / worktree
manifest, NEVER on a name regex (eight auto-named workers nearly
double-dispatched); a wake is confirmed by transcript content, never by a
state field. Third (x-cd1e): ``unclaimed`` is ADVISORY - the worker is fine,
the record is wrong - and never wakes or reroutes.
"""
from __future__ import annotations

import dataclasses
import functools
import json
import hashlib
import logging
import re
import shutil
import subprocess
import time
from collections import Counter, namedtuple
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

# The shipped tail classifier is the POSITIVE resumability marker: its
# ``stalled`` verdict asserts the session went silent while still owing its
# next move, which is a fact about the tail rather than an absence in it.
from fno.agents.session_truth import classify_tail

Verdict = namedtuple(
    "Verdict", "row_id name state verdict basis action agent", defaults=("claude",)
)
#: ``agent`` (default "claude") resolves the row's transcript store (x-c624);
#: apply lanes re-read the transcript through it, so it rides the verdict too.
Row = namedtuple("Row", "row_id name state node cwd agent", defaults=(None, "", "claude"))
#: ``records`` is [(epoch_s_or_None, text)] newest-last; ``tail_text`` is the
#: flattened join of those texts; ``last_role``/``last_text`` describe the LAST
#: record so the wake gate can run the shipped tail classifier (a POSITIVE
#: resumability marker - the absence of a 429 is not one). ``pr_polls`` is the
#: PR-activity view of the same window (tool_use commands and tool_result
#: bodies, which ``records`` drops), newest-last.
#: No transcript resolving -> None (ghost), which is a different fact from a
#: resolved-but-quiet transcript.
TailFacts = namedtuple(
    "TailFacts",
    "records last_event_epoch tail_text last_role last_text pr_polls",
    defaults=(None, "", ()),
)

GHOST = "ghost"
REROUTE = "reroute"
WAKE = "wake"
STALE = "stale"
LEAVE = "leave"
#: Advisory only (x-cd1e): the worker is fine, the RECORD is wrong. Never a
#: wake, never a reroute - the action lanes below switch on the specific
#: verdict, so this one cannot reach either of them. It replaces LEAVE so
#: the row surfaces in the digest, which is the whole point: nothing today
#: notices a live worker on a node no claim covers.
UNCLAIMED = "unclaimed"
SANDBOX_BLOCKED = "sandbox-blocked"
RECOVERABLE = "recoverable"
#: The keeper lane: a `--only keeper` filter value, not a row verdict -
#: keepers have no registry row by definition (a claimed keeper is LEAVE).
#: Discovery and the verdict live in :mod:`fno.agents.keeper_lane`.
KEEPER = "keeper"
#: Report-only friction verdicts (see docs/architecture/fleet-watchdog.md):
#: contended names a tree fact (two live rows, one linked worktree);
#: polling_settled names waste (PR-status reads after the tail showed the PR
#: MERGED or CLOSED). Neither acts at any apply level.
CONTENDED = "contended"
POLLING_SETTLED = "polling_settled"
#: Open node, spawn row, no crown, quiet past the drive threshold (x-c624). Driven like WAKE.
SILENCE = "silence"
#: Report-only: a past-ceiling row whose evidence says FINISHED work (node
#: shipped, or tail reads done) never enters the needs-human ask it can
#: never age out of. Evidence rules: docs/architecture/fleet-watchdog.md.
SPENT = "spent"

#: Every verdict this module can return. `--only` validates against THIS, not
#: a hand-copied tuple in the CLI - the copy went stale the moment a verdict
#: was added (`--only unclaimed` once exited 2 on a live verdict).
VERDICTS = frozenset({
    GHOST, REROUTE, WAKE, STALE, LEAVE, UNCLAIMED, RECOVERABLE, KEEPER,
    CONTENDED, POLLING_SETTLED, SANDBOX_BLOCKED, SILENCE, SPENT,
})

_RECOVERY_DURATION_RE = re.compile(r"^(\d+(?:\.\d+)?)([smhd])$", re.IGNORECASE)
MAX_RECOVERY_SINCE_S = 30 * 24 * 3600


def parse_recovery_since(value: str) -> float:
    """Parse a positive, bounded Codex recovery age such as ``24h``."""
    raw = str(value).strip().lower()
    match = _RECOVERY_DURATION_RE.fullmatch(raw)
    if match is None:
        raise ValueError(
            f"invalid since duration {value!r}; use a positive value with s, m, h, or d"
        )
    amount = float(match.group(1))
    seconds = amount * {"s": 1, "m": 60, "h": 3600, "d": 86400}[match.group(2)]
    if seconds <= 0 or seconds > MAX_RECOVERY_SINCE_S:
        raise ValueError(
            f"since duration {value!r} is outside the bounded 1s-30d recovery window"
        )
    return seconds


def resolve_recovery_cwd(value: Optional[str] = None) -> Path:
    """Resolve the exact checkout scope before touching the registry."""
    candidate = Path(value).expanduser() if value else Path.cwd()
    try:
        resolved = candidate.resolve(strict=True)
    except OSError as exc:
        raise ValueError(f"cwd {value or str(candidate)!r} could not be resolved: {exc}") from exc
    if not resolved.is_dir():
        raise ValueError(f"cwd {value or str(candidate)!r} is not a directory")
    return resolved


def scan_recoverable_codex_rollouts(cwd: Path, recency_seconds: float, *, now: Optional[float] = None):
    """Use the strict Codex rollout-minus-registry scanner for watchdog callers."""
    from fno.agents.discover import scan_recoverable_codex_rollouts as scan

    return scan(cwd, recency_seconds, now=now)


def run_recoverable_sweep(
    *,
    cwd: Path,
    recency_seconds: float,
    now_s: Optional[float] = None,
    scan_fn: Optional[Callable] = None,
    session_id: Optional[str] = None,
) -> tuple[dict, list[Row], Any]:
    """Build a non-destructive watchdog payload for Codex store recoverables."""
    now_s = now_s if now_s is not None else time.time()
    scan = (scan_fn or scan_recoverable_codex_rollouts)(
        cwd, recency_seconds, now=now_s
    )
    if session_id is not None:
        scan = dataclasses.replace(
            scan,
            recoverable=tuple(
                row for row in scan.recoverable if row.session_id == session_id
            ),
        )
    from fno.harness_identity import canonical_handle

    rows: list[Row] = []
    verdicts_out: list[Verdict] = []
    for candidate in scan.recoverable:
        handle = canonical_handle(candidate.session_id)
        rows.append(
            Row(candidate.session_id, handle, "orphaned", None, candidate.cwd, "codex")
        )
        usable = bool(candidate.transcript_usable)
        verdicts_out.append(
            Verdict(
                candidate.session_id,
                handle,
                "orphaned",
                RECOVERABLE,
                (
                    f"Codex rollout {candidate.rollout_path} is absent from the registry"
                    if usable
                    else (
                        f"Codex rollout {candidate.rollout_path} is unusable: "
                        f"{candidate.unusable_reason or 'transcript_unusable'}"
                    )
                ),
                "adopt" if usable else "refuse",
                "codex",
            )
        )
    complete = bool(scan.complete)
    counts = {RECOVERABLE: len(verdicts_out)} if complete else {}
    payload = {
        "generated_at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "verdicts": [v._asdict() for v in verdicts_out],
        "counts": counts,
        "warnings": list(scan.failures),
        "complete": complete,
        "scanned_count": scan.scanned_count,
        "malformed_count": scan.malformed_count,
        "unreadable_count": scan.unreadable_count,
        "cwd": str(cwd),
    }
    if session_id is not None:
        payload["selected_session_id"] = session_id
    if complete:
        payload["recoverable_count"] = len(verdicts_out)
        payload["usable_recoverable_count"] = scan.usable_recoverable_count
        payload["unusable_recoverable_count"] = (
            len(scan.recoverable) - scan.usable_recoverable_count
        )
    return payload, rows, scan


def _recovery_transcript_readback(candidate: Any) -> tuple[Optional[dict], Optional[str], str]:
    from fno.agents.discover import _codex_meta, _codex_rollout_usability

    if not candidate.transcript_usable:
        reason = candidate.unusable_reason or "no_readable_transcript_turn"
        return None, "transcript_unusable", f"transcript_unusable: {reason}"
    if not candidate.rollout_path.is_file():
        return (
            None,
            "transcript_unavailable",
            f"rollout vanished before adoption: {candidate.rollout_path}",
        )
    if _codex_meta(candidate.rollout_path) != (candidate.session_id, candidate.cwd):
        return (
            None,
            "transcript_changed",
            f"transcript_changed: rollout identity changed: {candidate.rollout_path}",
        )
    usable, last_event_at, last_turn_marker, unusable_reason = _codex_rollout_usability(
        candidate.rollout_path,
        session_id=candidate.session_id,
        cwd=candidate.cwd,
        sessions_dir=candidate.rollout_path.parent,
    )
    if not usable:
        return (
            None,
            "transcript_changed",
            f"transcript_changed: {unusable_reason or 'transcript_unusable'}",
        )
    if (
        last_event_at != candidate.last_event_at
        or last_turn_marker != candidate.last_turn_marker
    ):
        return (
            None,
            "transcript_changed",
            "transcript_changed: last event evidence no longer matches the scan",
        )
    return (
        {
            "transcript_usable": True,
            "last_event_at": last_event_at,
            "last_turn_marker": last_turn_marker,
        },
        None,
        "",
    )


def apply_recoverable(
    scan: Any,
    *,
    scope_cwd: Path,
    registry_path: Optional[Path] = None,
    adopt_fn: Optional[Callable] = None,
    confine_fn: Optional[Callable] = None,
    load_registry_fn: Optional[Callable] = None,
    update_registry_fn: Optional[Callable] = None,
    should_apply: Optional[Callable[[], bool]] = None,
) -> list[dict]:
    """Adopt a complete scan through the shared store-hit writer only."""
    if not scan.complete:
        reason = "; ".join(scan.failures) or "coverage could not be established"
        return [{
            "session_id": None,
            "outcome": "refused",
            "detail": f"recovery coverage incomplete: {reason}",
        }]

    from fno.agents import store_fallback
    from fno.agents.registry import load_registry, update_registry

    adopt = adopt_fn or store_fallback.adopt_store_hit
    confine = confine_fn or store_fallback.confine_store_hits
    load = load_registry_fn or load_registry
    update = update_registry_fn or update_registry
    results: list[dict] = []
    for index, candidate in enumerate(scan.recoverable):
        session_id = candidate.session_id
        if should_apply is not None and not should_apply():
            results.extend({
                "session_id": remaining.session_id,
                "outcome": "deferred",
                "detail": "tick budget spent; retry on the next tick",
            } for remaining in scan.recoverable[index:])
            break
        evidence, refusal_reason, refusal_detail = _recovery_transcript_readback(
            candidate
        )
        if refusal_reason is not None:
            results.append(
                {
                    "session_id": session_id,
                    "outcome": "refused",
                    "reason": refusal_reason,
                    "transcript_usable": False,
                    "detail": refusal_detail,
                }
            )
            continue
        try:
            from fno import paths

            hits = confine(
                session_id,
                [store_fallback.StoreHit("codex", session_id, candidate.cwd)],
                scope_cwd=str(scope_cwd),
                cross_project=False,
            )
            if len(hits) != 1:
                raise ValueError("project confinement did not return one verified hit")
            # The follow-up path rides the adoption write itself: patching it
            # in a second update afterward left a window where a failed patch
            # refused the batch while the row - missing its resume path -
            # stayed registered with no rollback.
            recovery_log_path = (
                paths.state_dir() / "agents" / session_id / "output.jsonl"
            )
            adopt(
                hits[0],
                registry_path=registry_path,
                token=session_id,
                log_path=str(recovery_log_path),
            )
            entries = load(registry_path)
            exact = [
                entry for entry in entries
                if getattr(entry, "harness", None) == "codex"
                and getattr(entry, "harness_session_id", None) == session_id
                and getattr(entry, "cwd", None) == candidate.cwd
                and getattr(entry, "origin", None) == "adopted"
            ]
            if len(exact) != 1:
                raise ValueError(
                    "registry did not contain exactly one adopted Codex row"
                )
            if getattr(exact[0], "log_path", None) != str(recovery_log_path):
                raise ValueError(
                    "adopted Codex row is missing its full-ID follow-up path"
                )
            post_evidence, post_reason, post_detail = _recovery_transcript_readback(
                candidate
            )
            if post_reason is not None:
                removed = 0

                def rollback_adopted_row(entries):
                    nonlocal removed
                    kept = []
                    for entry in entries:
                        if (
                            getattr(entry, "harness", None) == "codex"
                            and getattr(entry, "harness_session_id", None) == session_id
                            and getattr(entry, "cwd", None) == candidate.cwd
                            and getattr(entry, "origin", None) == "adopted"
                        ):
                            removed += 1
                            continue
                        kept.append(entry)
                    return kept

                update(rollback_adopted_row, path=registry_path)
                if removed != 1:
                    raise ValueError(
                        "registry rollback did not remove exactly one adopted Codex row"
                    )
                results.append(
                    {
                        "session_id": session_id,
                        "outcome": "refused",
                        "reason": post_reason,
                        "transcript_usable": False,
                        "registry_rollback": "removed",
                        "detail": post_detail,
                    }
                )
                continue
            results.append(
                {
                    "session_id": session_id,
                    "outcome": "applied",
                    **(post_evidence or evidence or {}),
                    "registry_row_count": len(exact),
                    "detail": f"adopted {session_id} handle={exact[0].name}",
                }
            )
        except Exception as exc:  # noqa: BLE001 - one vanished row never aborts the batch
            results.append({
                "session_id": session_id,
                "outcome": "refused",
                "reason": "adoption_failed",
                "transcript_usable": bool(evidence and evidence["transcript_usable"]),
                "detail": str(exc),
            })
    return results


def recovery_result_counts(results: Iterable[dict]) -> dict[str, int]:
    counts = {"applied": 0, "refused": 0, "deferred": 0}
    for result in results:
        outcome = result.get("outcome")
        if outcome in counts:
            counts[outcome] += 1
    return counts


#: How long a transcript must stay quiet before a session reads as finished
#: with its tree. Fallback when ``config.recovery.idle_threshold_seconds``
#: will not read; the claims-staleness and target-liveness readers share it.
QUIET_AFTER_S = 900

#: States that make a transcript-less row a ghost: the row claims a live-ish
#: session whose recorded id resolves to nothing. A ``stopped`` row with no
#: transcript is not a ghost - stopped is already the operator's answer.
#: ``busy``/``needs input`` are folded by ``_row_state``; the raw entries stay
#: because ``verdicts`` is pure and a caller that skips the fold must not
#: silently lose the ghost lane.
_GHOST_STATES = frozenset({"working", "busy", "blocked"})
#: Membership here is CANDIDACY, not a wake: the lane wakes only on
#: ``classify_tail == "stalled"`` (silent while still OWING its next move),
#: so a genuinely mid-task row cannot slip through whatever word the roster
#: wears. ``working`` belongs because a state word is what a session CLAIMS
#: and this lane exists because that claim lies (2026-08-18: a row read
#: ``working`` on an API error 56 minutes old; woken by hand, it opened a PR
#: fifteen minutes later). ``stopped`` belongs for the mirror reason: a
#: session that dies mid-turn is exactly a stopped row, and the stalled
#: check - not candidacy - is what guards a worker the operator stopped.
_WAKE_STATES = frozenset({"working", "blocked", "stopped"})

#: Graph statuses that mean the work is over, so an absent claim is the system
#: working rather than a gap. Read from the node, never from the row: the row
#: can still say `working` while the node is done, which is exactly the shape
#: that put completed work in the digest.
_FINISHED_NODE_STATUSES = frozenset({"done", "superseded", "deferred"})

#: Prefix marking a warning that leaves the LISTING USABLE: a reader deciding
#: whether it may trust a reading blocks on anything WITHOUT this, so an
#: unanticipated warning degrades safely by default (an allowlist of
#: known-harmless phrases got that polarity wrong twice). The two earners:
#: a latency notice (elapsed time on a probe that returned everything) and
#: an unmapped row state (a fidelity note on a row that IS in the result).
ADVISORY_WARNING_PREFIX = "roster advisory: "

#: Prefix of the headroom notice. It carries ADVISORY_WARNING_PREFIX because a
#: probe that took a while still returned every row.
HEADROOM_WARNING_PREFIX = f"{ADVISORY_WARNING_PREFIX}latency: "

#: The roster enumeration budget. ``claude agents --json --all`` is a
#: fleet-wide live-status probe (measured 3.4s on a 43-row fleet), so the
#: shared 3.0s interactive default times out, returns zero rows, and trips
#: ROSTER_REFUSAL on every tick - a watchdog that never sweeps while
#: reporting itself merely stale. No human waits on a tick: buy the fleet.
ROSTER_TIMEOUT_S = 30.0

#: Fraction of the roster budget that may be spent before the sweep warns.
ROSTER_HEADROOM = 0.5

#: Hard age ceiling on the wake lane (king ruling 2026-08-17): past it the
#: row reads ``stale`` and NEVER reaches an action lane - the 429 reset
#: stamp carries no date, so an old tail's time-of-day reading is garbage,
#: which would also poison reroute. Twelve hours, not twenty-four:
#: `parse_sgt_stamp` picks the nearest of yesterday/today/tomorrow, so a
#: date-less stamp is unambiguous for only half a day (measured: a reset
#: that truly passed 13h ago parses as 11h in the FUTURE and reads live).
WAKE_MAX_AGE_S = 12 * 3600

# Reset stamps ride the provider error text in Singapore local time (UTC+8):
# "02:48:21 SGT" is 18:48:21Z. Waking inside a closed window costs a real
# turn - which is why an UNPARSEABLE stamp classifies leave, never wake.
_RESET_STAMP_RE = re.compile(r"(\d{1,2}):(\d{2}):(\d{2})\s*(?:SGT|UTC\+8)")
_SGT_OFFSET_S = 8 * 3600
#: 60, not 15: a chatty attach must not push a still-live 429 out of the
#: window the wake gate reads - the burned turn inside a closed window is
#: the module's measured failure, and a 5-hour usage limit outlives any
#: 15-record tail.
_TAIL_RECORDS = 60
#: Confirmation scans deeper still, so a landed wake message is never read as
#: refused because the attach that followed it was chatty.
_CONFIRM_RECORDS = 120

#: The bare resume word (x-e21e): a bus-only row is woken with this and never
#: a message payload - a wake is an attach and a neutral resume, not a paste.
WAKE_MESSAGE = "continue"


# ---------------------------------------------------------------------------
# Rate-limit window math (pure)
# ---------------------------------------------------------------------------

def parse_sgt_stamp(
    hour: int, minute: int, second: int, now_s: float
) -> Optional[float]:
    """``HH:MM:SS SGT`` -> UTC epoch seconds, or None when out of range.

    A time-only stamp carries no date, so the day is the one of ``now`` rolled
    to the NEAREST candidate (a stamp 23h in the past is tomorrow's window, not
    yesterday's). A bad clock value (25:00) is None, never a guess.
    """
    if not (0 <= hour <= 23 and 0 <= minute <= 59 and 0 <= second <= 59):
        return None
    now_dt = datetime.fromtimestamp(now_s, tz=timezone.utc)
    base = now_dt.replace(hour=hour, minute=minute, second=second, microsecond=0)
    candidates = [
        base.timestamp() - _SGT_OFFSET_S + day * 86400 for day in (-1, 0, 1)
    ]
    return min(candidates, key=lambda t: abs(t - now_s))


def rate_limit_window(
    records: list, now_s: float
) -> tuple[str, Optional[float], str]:
    """``("none"|"live"|"passed"|"unknown", reset_epoch, stamp_str)`` over
    ``records`` (``[(epoch, text)]``, newest-last). A rate event is the NEWEST
    record the shipped classifier (:func:`fno.recovery.classify_session_error`)
    marks quota-swap-class - the sole integration point recovery itself uses,
    never narrowed by a prefilter: a quota body with no error-shaped words
    must still hold the window, and the accepted cost runs the other way -
    quoted prose can withhold a wake, because erring closed never burns a
    turn inside a live window. ``unknown`` (no parseable stamp) is fail-safe:
    the caller must not wake on it, and an older record's stamp never stands
    in for the newest one's."""
    from fno.recovery import classify_session_error

    for _epoch, text in reversed(records):
        err = classify_session_error(text)
        if err is None or not getattr(err, "triggers_swap", False):
            continue
        m = _RESET_STAMP_RE.search(text)
        if m is None:
            return "unknown", None, ""
        stamp = m.group(0)
        epoch = parse_sgt_stamp(
            int(m.group(1)), int(m.group(2)), int(m.group(3)), now_s
        )
        if epoch is None:
            return "unknown", None, stamp
        return ("live" if epoch > now_s else "passed"), epoch, stamp
    return "none", None, ""


# ---------------------------------------------------------------------------
# The classifier (pure)
# ---------------------------------------------------------------------------

def verdicts(
    rows: list[Row],
    *,
    transcript_for: Callable[[str], Optional[TailFacts]],
    claim_for: Callable[[str], dict],
    node_state_for: Callable[[str], Optional[dict]],
    now_s: float,
    provider_outages: Optional[dict[str, Any]] = None,
    worktree_check: Optional[Callable[[str], bool]] = None,
    pr_state_for: Optional[Callable[[str, int], Optional[str]]] = None,
    silence_after_s: Optional[float] = None,
) -> list[Verdict]:
    """One verdict per row, in table precedence (ghost > contended > silence
    > stale > reroute > sandbox-blocked > wake > leave). Each basis string
    names the measurement that decided it, so a reader can falsify the call.
    ``claim_for(node)`` returns the ``node:<id>`` claim view
    (``{"state", "holder"}``); ``node_state_for`` returns the graph entry or
    None. ``worktree_check`` defaults to the filesystem linked-worktree read;
    inject a stub. ``silence_after_s`` is None (disabled) unless a caller
    arms it."""
    facts_by_row: dict[str, Optional[TailFacts]] = {}
    for row in rows:
        try:
            facts_by_row[row.row_id] = transcript_for(row.row_id)
        except Exception:  # noqa: BLE001 - a failed read is never a verdict
            facts_by_row[row.row_id] = None

    # A row only earns REROUTE by appearing in an already-quorum-confirmed
    # breaker (_breakers() in provider_outage.py refuses to emit one below
    # policy.quorum distinct rows) - never from this row's own 429 alone.
    quorum_row_ids = frozenset(
        row_id
        for breaker in (provider_outages or {}).get("breakers") or []
        for row_id in breaker.get("row_ids") or []
    )

    # Contention reads the TREE, not the row: occupied is the default and a
    # shared checkout is coordination, not contention (fleet-watchdog.md).
    worktree_check = worktree_check or _is_linked_worktree
    live_in_tree: dict[str, list[str]] = {}
    for row in rows:
        if (
            row.cwd
            and worktree_check(row.cwd)
            and not finished_with_the_tree(
                facts_by_row.get(row.row_id), now_s, QUIET_AFTER_S
            )
        ):
            live_in_tree.setdefault(row.cwd, []).append(row.row_id)

    out: list[Verdict] = []
    for row in rows:
        occupants = live_in_tree.get(row.cwd, ())
        # Only a live occupant reports; a finished row beside a live one is not contention.
        peers = (
            tuple(sid for sid in occupants if sid != row.row_id)
            if row.row_id in occupants else ()
        )
        verdict = _verdict_one(
            row,
            facts=facts_by_row.get(row.row_id),
            claim_for=claim_for,
            node_state_for=node_state_for,
            now_s=now_s,
            in_quorum_breaker=row.row_id in quorum_row_ids,
            peers=peers,
            silence_after_s=silence_after_s,
        )
        # polling_settled upgrades a LEAVE like unclaimed: liveness outranks waste.
        if verdict.verdict == LEAVE:
            facts = facts_by_row.get(row.row_id)
            if facts is not None:
                poll = _polling_basis(facts.pr_polls, pr_state_for, row.cwd)
                if poll is not None:
                    n, state, count = poll
                    verdict = verdict._replace(
                        verdict=POLLING_SETTLED,
                        basis=(
                            f"{count} PR-status reads of #{n} after the "
                            f"tail read it {state}"
                        ),
                        action="report",
                    )
        # The unclaimed advisory upgrades a LEAVE HERE, not at a leave return
        # (there are four, and guarding one left the healthy common case
        # uncovered). Only LEAVE: every other verdict says something louder,
        # and burying it under a record-keeping note trades real signal for
        # an advisory.
        if verdict.verdict == LEAVE:
            unclaimed_basis = _unclaimed_node_basis(row, claim_for, node_state_for)
            if unclaimed_basis:
                verdict = verdict._replace(
                    verdict=UNCLAIMED, basis=f"{verdict.basis}; {unclaimed_basis}"
                )
        out.append(verdict)
    return out


def lane_armed(settings: Any) -> bool:
    """Will the tick actually sweep? One condition, every reader: a freshness
    reader that does not share the producer's own condition once printed
    FLEET WATCHDOG STALE for a cadence that was off on purpose."""
    return _armed(settings, None)


def handoff_armed(settings: Any) -> bool:
    """Return true only for the explicit cross-provider action level."""
    return _armed(settings, "handoff")


def wake_armed(settings: Any) -> bool:
    """Return true only for the level that may resume a stalled session."""
    return _armed(settings, "wake")


def _armed(settings: Any, mode: Optional[str]) -> bool:
    """The lane's arming question, spelled once. `mode` None asks only whether
    the lane runs at all; a mode asks for that exact depth."""
    try:
        watchdog = settings.recovery.watchdog
        return bool(
            watchdog.enabled
            and (mode is None or watchdog.mode == mode)
            and settings.recovery.enabled
            and settings.autonomy.enabled
        )
    except Exception:  # noqa: BLE001 - a partial settings stub is not armed
        return False


def _unknown_provider_report(reason: str) -> dict[str, Any]:
    return {
        "instrument": "unknown",
        "breakers": [],
        "counts": {reason: 1},
        "refusals": [{"reason": reason, "count": 1}],
    }


def finished_with_the_tree(
    facts: Optional[TailFacts], now_s: float, quiet_after_s: float
) -> bool:
    """Is this session done with its worktree? One question, one answer.

    The claims-staleness reader and the daemon's retirement grace ask the
    same question, so they share one derivation: quiet past the bar AND a
    tail that is not still in play. False is the safe answer and every
    unreadable input returns it.
    """
    if facts is None or facts.last_event_epoch is None:
        return False
    age_s = max(0.0, now_s - facts.last_event_epoch)
    if age_s <= quiet_after_s:
        return False
    return classify_tail(
        facts.last_role, facts.last_text, age_s
    ) not in _ENGAGED_TAILS


def _mins(now_s: float, epoch: Optional[float]) -> Optional[int]:
    if epoch is None:
        return None
    return max(0, int((now_s - epoch) / 60))


def _age_clause(now_s: float, epoch: Optional[float]) -> str:
    n = _mins(now_s, epoch)
    return f"{n}m" if n is not None else "unknown"


def _verdict(row: Row, verdict: str, basis: str, action: str) -> Verdict:
    return Verdict(row.row_id, row.name, row.state, verdict, basis, action, row.agent)


def _spent_basis(
    row: Row,
    facts: Optional[TailFacts],
    facts_age_s: float,
    *,
    node_state_for: Callable[[str], Optional[dict]],
) -> Optional[str]:
    """Positive done evidence that a past-ceiling row is finished work, or None."""
    age = f"quiet {int(facts_age_s // 3600)}h past the wake ceiling"
    if row.node:
        try:
            node_state = node_state_for(row.node) or {}
        except Exception:  # noqa: BLE001 - a failed read is never evidence
            node_state = {}
        status = str(node_state.get("status") or "").lower()
        if status in _FINISHED_NODE_STATUSES:
            return f"node {row.node} {status}; {age}; finished row, nothing to triage"
    if facts is not None and classify_tail(
        facts.last_role, facts.last_text, facts_age_s
    ) == "done":
        return f"tail reads done; {age}; finished row, nothing to triage"
    return None


def _branch_commit_count(cwd: str) -> Optional[int]:
    """Count commits on the checked-out branch beyond ``origin/main``."""
    if not cwd:
        return None
    try:
        proc = subprocess.run(
            ["git", "-C", cwd, "rev-list", "--count", "origin/main..HEAD"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        return int(proc.stdout.strip())
    except (TypeError, ValueError):
        return None


def _sandbox_denial_text(facts: Optional[TailFacts]) -> Optional[str]:
    """Return the last distress text only for the Git sandbox signature."""
    if facts is None:
        return None
    text = facts.last_text or facts.tail_text
    if "Operation not permitted" not in text:
        return None
    if re.search(r"(?<![\w.])\.git[/\\]", text) is None:
        return None
    return text


def _sandbox_blocked_verdict(
    row: Row,
    *,
    facts: Optional[TailFacts],
    claim_for: Callable[[str], dict],
) -> Optional[Verdict]:
    """Classify a Codex Git denial only when both reaping guards are clear."""
    evidence = _sandbox_denial_text(facts)
    if evidence is None:
        return None
    if not row.node:
        return Verdict(
            row.row_id, row.name, row.state, LEAVE,
            "sandbox denial held: node identity unreadable; no reap", "none",
        )
    try:
        claim = claim_for(row.node)
    except Exception as exc:  # noqa: BLE001 - unreadable claims never authorize reap
        return Verdict(
            row.row_id, row.name, row.state, LEAVE,
            f"sandbox denial held: claim unreadable ({exc}); no reap", "none",
        )
    if claim.get("state") != "free":
        state = claim.get("state") or "unknown"
        holder = claim.get("holder") or "unknown"
        return Verdict(
            row.row_id, row.name, row.state, LEAVE,
            f"sandbox denial held: node claim {state} ({holder}); no reap", "none",
        )
    commit_count = _branch_commit_count(row.cwd)
    if commit_count is None:
        return Verdict(
            row.row_id, row.name, row.state, LEAVE,
            "sandbox denial held: branch commit count unreadable; no reap", "none",
        )
    if commit_count:
        return Verdict(
            row.row_id, row.name, row.state, LEAVE,
            f"sandbox denial held: branch carries {commit_count} commit(s); no reap",
            "none",
        )
    return Verdict(
        row.row_id,
        row.name,
        row.state,
        SANDBOX_BLOCKED,
        f"Codex sandbox blocked Git writes: {evidence}; node claim free; branch commits=0",
        "reap",
    )


def _verdict_one(
    row: Row,
    *,
    facts: Optional[TailFacts],
    claim_for: Callable[[str], dict],
    node_state_for: Callable[[str], Optional[dict]],
    now_s: float,
    in_quorum_breaker: bool = False,
    peers: tuple[str, ...] = (),
    silence_after_s: Optional[float] = None,
) -> Verdict:

    # ghost: claims working/blocked, no transcript resolves for the id.
    if facts is None and row.state in _GHOST_STATES:
        basis = (
            f"no transcript for {row.row_id}"
            if row.agent in {"claude", "codex"}
            else f"harness {row.agent} keeps no per-session transcript, unmeasurable"
        )
        return _verdict(row, GHOST, basis, "report")

    # contended: below ghost (liveness outranks a tree fact), report-only.
    if peers:
        return _verdict(
            row, CONTENDED,
            f"worktree {row.cwd} holds {len(peers) + 1} live sessions, "
            f"peers {'/'.join(peers)}",
            "report",
        )

    # silence (x-c624): open node, quiet past the drive threshold; node_state_for gated on age.
    if silence_after_s is not None and facts is not None and facts.last_event_epoch is not None:
        silence_age_s = max(0.0, now_s - facts.last_event_epoch)
        if silence_age_s > silence_after_s:
            try:
                node_state = node_state_for(row.node) if row.node else None
            except Exception:  # noqa: BLE001 - unreadable graph condemns nothing
                return _verdict(row, LEAVE,
                                "graph unreadable, silence verdict refused", "none")
            node_open = node_state is not None and str(node_state.get("status") or "") not in ("done", "superseded")
            if node_open:
                return _verdict(
                    row, SILENCE,
                    f"open node {row.node}, transcript quiet {_mins(now_s, facts.last_event_epoch)}m", "drive",
                )

    window, reset_epoch, stamp = ("none", None, "")
    if facts is not None:
        window, reset_epoch, stamp = rate_limit_window(facts.records, now_s)

    # stale: the hard age ceiling, BEFORE the 429 window math - a date-less
    # reset stamp's time-of-day reading is garbage on an old tail and would
    # poison reroute below.
    facts_age_s: Optional[float] = None
    if facts is not None and facts.last_event_epoch is not None:
        facts_age_s = max(0.0, now_s - facts.last_event_epoch)
    if row.state in _WAKE_STATES and facts_age_s is not None:
        if facts_age_s > WAKE_MAX_AGE_S:
            spent = _spent_basis(row, facts, facts_age_s, node_state_for=node_state_for)
            if spent is not None:
                return _verdict(row, SPENT, spent, "none")
            return _verdict(
                row, STALE,
                f"{row.state} {int(facts_age_s // 3600)}h old, past the "
                f"{int(WAKE_MAX_AGE_S // 3600)}h wake ceiling, needs a human",
                "report",
            )

    # reroute: blocked on a 429 whose window has NOT opened. A single 429 is
    # terminal for this session but is not provider-wide authority: the
    # durable fold requires quorum rows before a migration lane may act.
    # Waking bounces (proved twice by hand).
    if (
        row.state == "blocked"
        and window == "live"
        and reset_epoch is not None
        # A tail with no parsed timestamp skips the age ceiling above; the
        # louder lane must not be the laxer one on a row of unknown age.
        and facts is not None
        and facts.last_event_epoch is not None
    ):
        if in_quorum_breaker:
            return _verdict(
                row, REROUTE,
                "429 terminal for this session; provider quorum already "
                "confirmed by a separate breaker row",
                "redispatch",
            )
        return _verdict(
            row, LEAVE,
            "429 terminal for this session; waiting for positive provider quorum",
            "none",
        )

    sandbox_verdict = _sandbox_blocked_verdict(
        row, facts=facts, claim_for=claim_for
    )
    if sandbox_verdict is not None:
        return sandbox_verdict

    # wake: blocked or stopped, a transcript exists, and no live 429 window.
    # Every condition is POSITIVE evidence (king ruling 2026-08-17): an age
    # under the ceiling, a parseable last event, and classify_tail
    # ``stalled`` - silent while still owing its next move. "No 429 in tail"
    # is an absence and never a wake reason.
    if row.state in _WAKE_STATES and facts is not None:
        if facts.last_event_epoch is None:
            return _verdict(row, LEAVE,
                            "no parseable transcript evidence, not wakeable",
                            "none")
        if window == "unknown":
            return _verdict(row, LEAVE,
                            f"429 present, reset window unknown "
                            f"({stamp or 'no stamp'})", "none")
        if window == "live" and reset_epoch is not None:
            reset_utc = datetime.fromtimestamp(reset_epoch, tz=timezone.utc)
            return _verdict(row, LEAVE,
                            f"429 resets {reset_utc.strftime('%H:%M:%SZ')}, "
                            f"window not open", "none")
        truth = classify_tail(facts.last_role, facts.last_text, facts_age_s)
        if truth != "stalled":
            return _verdict(row, LEAVE,
                            f"tail reads {truth}, session does not owe a move",
                            "none")
        clause = ("last 429 window passed" if window == "passed"
                  else "silent, no 429 in tail")
        return _verdict(row, WAKE,
                        f"{row.state} {_mins(now_s, facts.last_event_epoch)}m "
                        f"silent, {clause}", "resume")

    # leave: everything else, including every healthy injectable row - the
    # watchdog never competes with the normal inject path. Never
    # "reachable": nothing here probes reachability. The basis states what
    # was measured - the age of the last write and the roster's state claim,
    # two facts that can disagree, and did.
    basis = (
        f"state {row.state}, last turn "
        f"{_age_clause(now_s, facts.last_event_epoch)} ago, no lane applies"
        if facts is not None
        else f"no transcript, state {row.state}"
    )
    return _verdict(row, LEAVE, basis, "none")


def _unclaimed_node_basis(
    row: Row,
    claim_for: Callable[[str], dict],
    node_state_for: Callable[[str], Optional[dict]],
) -> str:
    """Is this live row working a node that no claim covers? BLIND SPOT, beside
    the two the module header records: a row whose node did not resolve carries
    ``node=None`` and cannot be checked, and that is PRECISELY the shape of a
    worker that never ran ``fno do target init`` (``Row.node`` comes from the
    worktree manifest then the session-keyed ledger, both written downstream of
    init) - so this catches the manifest-written-but-unclaimed case only.
    Everything else degrades to silence: an advisory that fires on an
    unreadable store trains its reader to ignore it."""
    if not row.node or row.state in _TERMINAL_STATES:
        # A finished row's claim was CORRECTLY released, and
        # `claude agents --json --all` keeps terminal rows forever - flagging
        # any of them (not just `done`: `completed`/`exited`/`killed` are the
        # same noise) accumulates permanent digest noise.
        return ""
    try:
        claim = claim_for(row.node)
    except Exception:  # noqa: BLE001 - a failed read is never a finding
        return ""
    # PLAIN `free` only; `stale` is the NORMAL reading for a healthy worker
    # parked in a CI wait (the heartbeat is tool-call driven), so flagging it
    # would put a working fleet in the digest every tick.
    if claim.get("state") != "free":
        return ""
    # A FINISHED node has no claim because the work is over and the claim was
    # released, which is the system behaving. Reporting it is the same permanent
    # noise the terminal-row skip above exists to prevent, arriving by the other
    # door: the row still reads `working` while the graph says done, so it
    # reaches this LEAVE and gets upgraded. An unreadable node state answers
    # nothing and the advisory stays silent, because a report built on a failed
    # read trains its reader to ignore the report.
    try:
        node_state = node_state_for(row.node) or {}
    except Exception:  # noqa: BLE001 - a failed read is never a finding
        return ""
    if str(node_state.get("status") or "").lower() in _FINISHED_NODE_STATUSES:
        return ""
    return f"node {row.node} carries NO claim while this row is live"


#: The tail-read window: transcripts grow without bound, and the classifier
#: only ever asks about the last turn, so read the trailing 256 KiB, not the file.
_TICK_TAIL_BYTES = 256 * 1024


def tail_entries(
    session_id: str,
    cwd: str,
    *,
    agent: str,
) -> Optional[list[dict]]:
    """Resolve a session's transcript and return its parsed tail records.

    THE one transcript read per tick: the tail classifier, the outage
    collector and the poll detector all derive from this single parse
    (measured defect: every transcript was read twice per tick). ``agent``
    names the harness whose transcript store holds the session - there is
    deliberately no default, because a defaulted read silently opens the
    claude store for every other harness and reads every such worker as a
    ghost. None means the transcript could not be resolved, read, or decoded
    - the caller renders that downstream (ghost facts, or a named refusal).
    A torn or foreign JSONL line is skipped, not fatal.
    """
    from fno.provenance.observed import resolve_transcript_path

    try:
        path = resolve_transcript_path(agent, session_id, cwd)
    except Exception:  # noqa: BLE001 - a broken resolver is "no transcript"
        return None
    if path is None:
        return None
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            fh.seek(max(0, size - _TICK_TAIL_BYTES))
            chunk = fh.read()
        lines = chunk.decode("utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        return None
    if size > _TICK_TAIL_BYTES and lines:
        lines = lines[1:]  # a mid-file seek lands inside a line; drop it
    parsed: list[dict] = []
    for line in lines:
        try:
            record = json.loads(line)
        except Exception:  # noqa: BLE001 - a torn/foreign line is not data
            continue
        if isinstance(record, dict):
            parsed.append(record)
    return parsed


def _record_epoch(record: dict) -> Optional[float]:
    ts = record.get("timestamp")
    if not ts:
        return None
    try:
        return datetime.fromisoformat(str(ts).replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def _facts_from_entries(
    entries: Optional[list[dict]], max_records: int
) -> Optional[TailFacts]:
    """Pure derivation of :class:`TailFacts` from a parsed transcript tail."""
    if entries is None:
        return None
    windowed: list[tuple[Optional[float], str, Optional[str], tuple]] = []
    for record in entries:
        text = _record_text(record)
        msg = record.get("message")
        role = msg.get("role") if isinstance(msg, dict) else None
        windowed.append(
            (_record_epoch(record), text, str(role) if role else None,
             _pr_poll_record(record))
        )
    # The window bounds EVERYTHING downstream, the (role, text) pair included:
    # a pair read from a record older than max_records would pair a stale text
    # with the fresh age and window inputs it is classified against.
    window = windowed[-max_records:]
    records = [(epoch, text) for epoch, text, _role, _pr in window]
    last_epoch = next((t for t, _ in reversed(records) if t is not None), None)
    last_role: Optional[str] = None
    last_text = ""
    for _epoch, text, role, _pr in reversed(window):
        if role:
            # The LAST role-bearing record inside the window decides the tail
            # classifier's input; a trailing user turn clears stale assistant
            # signals.
            last_role, last_text = role, text
            break
    pr_polls = tuple(fact for *_heads, facts in window for fact in facts)
    return TailFacts(
        records, last_epoch, " ".join(t for _, t in records),
        last_role, last_text, pr_polls,
    )


def tail_facts(
    session_id: str,
    cwd: str,
    *,
    agent: str,
    max_records: int = _TAIL_RECORDS,
) -> Optional[TailFacts]:
    """Resolve a session's transcript and tail-read it. Never raises. A
    missing transcript is None, which the classifier renders as a fact
    (ghost / unknown-age), never as fresh. ``agent`` is required - see
    :func:`tail_entries`. A caller holding its entries already (the tick
    shares one read) should derive with :func:`_facts_from_entries` instead
    of reading again.
    """
    return _facts_from_entries(tail_entries(session_id, cwd, agent=agent), max_records)


@functools.lru_cache(maxsize=1)
def _harness_by_session(registry_path: str) -> dict[str, str]:
    """Session id -> harness, from the registry. Keyed on the declared
    registry path (the state root this reader depends on), so a re-pointed
    state root cannot read a stale map. Cached for the life of the process:
    every caller here is a one-shot CLI command, and the watchdog daemon
    reads the harness off Row instead."""
    from fno.agents.registry import load_registry

    try:
        entries = list(load_registry())
    except Exception:  # noqa: BLE001 - an unreadable registry answers claude
        return {}
    return {
        str(getattr(e, "harness_session_id", "") or ""): str(
            getattr(e, "harness", "") or "claude"
        )
        for e in entries
        if getattr(e, "harness_session_id", None)
    }


def harness_for_session(session_id: str) -> str:
    """The harness that owns this session, or claude when the registry does
    not know it. The ONE place that answers "which harness is this session"
    for a caller holding only an id."""
    from fno.paths import agents_registry_path

    try:
        key = str(agents_registry_path())
    except Exception:  # noqa: BLE001 - an unreadable state root answers claude
        return "claude"
    return _harness_by_session(key).get(session_id, "claude")


def _record_text(e: dict) -> str:
    """Flattened text of one transcript record (message bodies and top-level
    system text - a provider 429 lands in either)."""
    msg = e.get("message")
    parts: list[str] = []
    content = msg.get("content") if isinstance(msg, dict) else None
    if isinstance(content, str):
        parts = [content]
    elif isinstance(content, list):
        parts = [
            p.get("text", "")
            for p in content
            if isinstance(p, dict) and p.get("type") == "text"
        ]
    if not parts and isinstance(e.get("text"), str):
        parts = [e["text"]]
    return " ".join(" ".join(parts).split())


#: A command that reads one PR's status; ``fno do pr wait`` never reaches the transcript, so a repeat is re-asking.
_PR_READ_RE = re.compile(
    r"\b(?:fno\s+do\s+pr|gh\s+pr)\s+(?:status|info|view|checks|wait)\s+#?(\d+)"
)
_PR_JSON_STATE_RE = re.compile(r'"state"\s*:\s*"(MERGED|CLOSED)"')
#: A PR number inside a tool-result blob (``"pr":1371``, ``pulls/1371``).
_PR_ID_IN_BLOB_RE = re.compile(r'(?:pr|pulls|number)[/":# =]+(\d+)', re.I)


def _pr_poll_record(e: dict) -> tuple[tuple[str, int, str], ...]:
    """PR facts from one raw record: ("read", n, "") for a status-read
    command, ("settled", n, terminal) for a tool result asserting it - the
    two places ``_record_text`` drops."""
    msg = e.get("message")
    content = msg.get("content") if isinstance(msg, dict) else None
    blobs: list[str] = []
    if isinstance(content, list):
        for part in content:
            if not isinstance(part, dict):
                continue
            if part.get("type") == "tool_use":
                inp = part.get("input")
                if isinstance(inp, dict):
                    blobs.append(str(inp.get("command") or ""))
            elif part.get("type") == "tool_result":
                inner = part.get("content")
                if isinstance(inner, str):
                    blobs.append(inner)
                elif isinstance(inner, list):
                    blobs.append(" ".join(
                        p.get("text", "")
                        for p in inner
                        if isinstance(p, dict) and p.get("type") == "text"
                    ))
    facts: list[tuple[str, int, str]] = []
    for blob in blobs:
        for m in _PR_READ_RE.finditer(blob):
            facts.append(("read", int(m.group(1)), ""))
        state = _PR_JSON_STATE_RE.search(blob)
        if state:
            for n in sorted(set(
                int(x) for x in _PR_ID_IN_BLOB_RE.findall(blob)
            )):
                facts.append(("settled", n, state.group(1)))
    return tuple(facts)


def _polling_basis(
    pr_polls: tuple,
    pr_state_for: Optional[Callable[[str, int], Optional[str]]],
    cwd: str,
) -> Optional[tuple[int, str, int]]:
    """(pr, state, reads-after-settle) when the tail keeps reading a PR it
    itself shows settled, else None. The settle marker must be IN the tail;
    the live read must confirm terminal NOW, and unreadable is UNKNOWN,
    which never produces a verdict."""
    settled: dict[int, str] = {}
    after: dict[int, int] = {}
    for kind, n, state in pr_polls:
        if kind == "settled":
            settled.setdefault(n, state)
        elif n in settled:
            after[n] = after.get(n, 0) + 1
    for n, count in after.items():
        if count < 2:
            continue
        current = pr_state_for(cwd, n) if pr_state_for is not None else None
        if current is not None and current.upper() in ("MERGED", "CLOSED"):
            return n, current.upper(), count
    return None


def _ledger_nodes() -> dict[str, str]:
    """``{claude session id -> node id}`` from the execution ledger - the
    machine-written join for a canonical-checkout worker with no worktree
    manifest, where names are ambiguous (the ``t-`` shorthand strips the
    dash; the name-join trap). Each entry names its node in
    ``graph_node_id``; ``title`` is free text and joins nothing. A miss
    degrades to no node, which condemns nothing."""
    from fno import paths
    from fno.graph.types import normalize_graph_node_id

    try:
        entries = json.loads(
            paths.ledger_json().read_text(encoding="utf-8")
        ).get("entries", [])
    except (OSError, json.JSONDecodeError, UnicodeDecodeError, AttributeError):
        return {}
    out: dict[str, str] = {}
    for e in entries:
        if not isinstance(e, dict):
            continue
        node = normalize_graph_node_id(e.get("graph_node_id"))
        if not node:
            continue
        # `sessions` is the plural spelling; older entries record a single
        # `session_id`, and a bare string under either key must not be
        # SPREAD (that maps one node onto every character of the id).
        # ledger_join.py guards both shapes; this join is worthless if it
        # silently drops the rows that one keeps.
        raw = e.get("sessions") or e.get("session_id") or []
        for sid in ([raw] if isinstance(raw, str) else raw):
            if sid and isinstance(sid, str):
                out[sid] = node
    return out


def fleet_rows(*, timeout: Optional[float] = None) -> tuple[list[Row], list[str]]:
    """Enumerate the fleet from ``claude agents --json --all`` joined to the
    registry for recorded identity. Node identity is the registry's spawn-time
    stamp first, then a linked-worktree manifest, then the session-keyed ledger
    for legacy or unstamped rows. All three are machine-written; never parse a
    node from the row name."""
    from fno.agents.harnesses.claude import claude_agents_rows
    from fno.agents.registry import load_registry
    from fno.recovery import _node_id_from_worktree

    # A caller running INSIDE a bounded tick spends its remaining budget,
    # never this lane's own: the tick's wall-clock deadline is the shorter of
    # the two, and blowing it exits 75 and kills every later leg. Unbounded
    # callers (the manual sweep) get the full fleet budget.
    budget = ROSTER_TIMEOUT_S if timeout is None else max(1.0, min(timeout, ROSTER_TIMEOUT_S))
    probe_started = time.time()
    raw, warnings = claude_agents_rows(timeout=budget)
    elapsed = time.time() - probe_started
    if elapsed > budget * ROSTER_HEADROOM:
        # A fixed budget measured against a GROWING fleet fails silently on
        # the day the fleet outgrows it: the probe times out, the sweep reads
        # zero rows, and the refusal reads as a broken fleet rather than a
        # budget that needs raising. Nothing else warns, so the approach to
        # the line has to be the thing that speaks.
        warnings = [
            *warnings,
            f"{HEADROOM_WARNING_PREFIX}took {elapsed:.1f}s of its {budget:.0f}s "
            f"budget for {len(raw)} row(s); past the budget the sweep reads zero "
            f"rows and refuses. Raise ROSTER_TIMEOUT_S",
        ]
    by_sid: dict[str, Any] = {}
    registry_rows: list[Any] = []
    try:
        registry_rows = list(load_registry())
        for entry in registry_rows:
            if getattr(entry, "harness", None) != "claude":
                continue
            sid = (
                getattr(entry, "harness_session_id", None)
                or getattr(entry, "cc_session_id", None)
            )
            if sid:
                by_sid[str(sid)] = entry
    except Exception:  # noqa: BLE001 - registry read miss degrades to claude rows
        by_sid = {}
    # None = not read yet; a read that maps nothing must not re-read the
    # multi-megabyte ledger once per manifest-less row.
    ledger_nodes: Optional[dict[str, str]] = None
    out: list[Row] = []
    unmapped_states: set[str] = set()
    skipped_no_sid = 0
    for r in raw:
        # Both spellings: claude renamed `short_id`/`status` once already,
        # which is why the sibling parser resolves every field through
        # aliases. A rename here zeroes the roster and fires the refusal on
        # every tick, with a warning that blames the instrument.
        sid = str(r.get("sessionId") or r.get("session_id") or "")
        if not sid:
            # A row carrying only claude's 8-hex short id can never resolve a
            # transcript, a claim, or a ledger row - carrying it forward reads
            # a live session as a ghost. Skipped loudly, never classified.
            skipped_no_sid += 1
            continue
        match: Any = by_sid.get(sid)
        name = str(getattr(match, "name", None) or r.get("name") or sid)
        cwd = str(r.get("cwd") or getattr(match, "cwd", "") or "")
        # For an unstamped legacy row, the manifest is per-ROW identity only
        # when the cwd is that row's own linked worktree. On a shared checkout
        # every session reads the SAME graph_node_id, so only the session-keyed
        # ledger is a truthful fallback; a miss condemns nothing.
        state, state_warning = _row_state(r)
        if state_warning:
            unmapped_states.add(state_warning)
        # The registry stamp is the spawn-time identity for this exact row.
        # Manifest and ledger reads remain fallbacks for legacy/unstamped rows.
        node = getattr(match, "node", None)
        if node is None and _is_linked_worktree(cwd):
            node = _node_id_from_worktree(cwd)
        if node is None:
            if ledger_nodes is None:
                ledger_nodes = _ledger_nodes()
            node = ledger_nodes.get(sid)
        out.append(Row(
            row_id=sid,
            name=name,
            state=state,
            node=node,
            cwd=cwd,
        ))
    # The Claude roster is a harness-specific instrument. Registry rows are
    # the authoritative fallback for other harnesses, especially Codex thread
    # workers whose app-server has no row in `claude agents --json --all`.
    from fno.agents.spawn_gate import LIVE_STATUSES

    seen_row_ids = {row.row_id for row in out}
    skipped_nonclaude_no_id = 0
    for entry in registry_rows:
        if getattr(entry, "harness", None) == "claude":
            continue
        if getattr(entry, "status", None) not in LIVE_STATUSES:
            continue
        row_id = (
            getattr(entry, "harness_session_id", None)
            or getattr(entry, "session_id", None)
            or getattr(entry, "short_id", None)
        )
        if not row_id:
            # Same discipline as the claude roster above: a row carrying only
            # a name can never resolve a transcript or a claim. Falling back
            # to the name would silently drop a same-named live row at the
            # dedup below, so it is skipped loudly instead.
            skipped_nonclaude_no_id += 1
            continue
        if str(row_id) in seen_row_ids:
            continue
        row_id = str(row_id)
        # Registry statuses fold through the SAME mapper the claude roster
        # uses, so lane predicates never gate on a vocabulary only one
        # producer speaks. An unmapped spelling is carried loudly, never
        # silently dropped out of every lane set.
        state, state_warning = _row_state({"status": str(getattr(entry, "status", "") or "")})
        if state_warning:
            unmapped_states.add(state_warning)
        out.append(
            Row(
                row_id=row_id,
                name=str(getattr(entry, "name", None) or row_id),
                state=state,
                node=getattr(entry, "node", None),
                cwd=str(getattr(entry, "cwd", "") or ""),
                # The loop exists only because this entry is NOT claude; the
                # harness it filtered on is the one the row must carry.
                agent=str(getattr(entry, "harness", "") or "claude"),
            )
        )
        seen_row_ids.add(row_id)
    if skipped_no_sid:
        warnings = [
            *warnings,
            f"{skipped_no_sid} row(s) carried no session id, unmeasurable, skipped",
        ]
    if skipped_nonclaude_no_id:
        warnings = [
            *warnings,
            f"{skipped_nonclaude_no_id} non-claude row(s) carried no session id, "
            "unmeasurable, skipped",
        ]
    warnings = [*warnings, *sorted(unmapped_states)]
    return out, warnings


def silence_rows(roots: "Iterable[Path]") -> tuple[list[Row], list[str]]:
    """SILENCE's row set (x-c624): the registry, not ``claude agents --json``."""
    from fno.agents.registry import load_registry
    from fno.agents.spawn_gate import LIVE_STATUSES

    root_strs = [str(Path(r).resolve()) for r in roots]
    out: list[Row] = []
    try:
        entries = list(load_registry())
    except Exception as exc:  # noqa: BLE001 - an unreadable registry scopes nothing
        return [], [f"registry unreadable, silence sweep refused: {exc!r}"]
    for e in entries:
        live = getattr(e, "status", None) in LIVE_STATUSES
        spawn = getattr(e, "origin", None) == "spawn" and getattr(e, "crown_level", None) is None
        node = getattr(e, "node", None)
        scope = str(getattr(e, "project_root", "") or getattr(e, "cwd", "") or "")
        if not (live and spawn and node and scope):
            continue
        try:
            resolved = str(Path(scope).resolve())
        except OSError:
            continue
        if not any(resolved == r or resolved.startswith(r + "/") for r in root_strs):
            continue
        row_id = str(getattr(e, "harness_session_id", None) or getattr(e, "short_id", None) or e.name)
        cwd, agent = str(getattr(e, "cwd", "") or ""), str(getattr(e, "harness", "") or "claude")
        state = str(getattr(e, "status", "") or "")
        out.append(Row(row_id, str(e.name), state, str(node), cwd, agent))
    return out, []


def silence_verdicts(
    roots: "Iterable[Path]",
    *,
    now_s: Optional[float] = None,
    silence_after_s: Optional[float] = None,
    claim_for: Optional[Callable[[str], dict]] = None,
    node_state_for: Optional[Callable[[str], Optional[dict]]] = None,
) -> tuple[list[Verdict], list[Row]]:
    """SILENCE's classify step: :func:`silence_rows`, then :func:`verdicts` with the age gate."""
    from fno.agents.session_truth import resolve_session_truth
    from fno.config import load_settings

    now_s = now_s if now_s is not None else time.time()
    if silence_after_s is None:
        try:
            silence_after_s = float(load_settings().recovery.idle_threshold_seconds)
        except Exception:  # noqa: BLE001 - an unreadable config never arms a guess
            silence_after_s = 900.0
    rows, _warnings = silence_rows(roots)
    if not rows:
        return [], []

    def transcript_for(row_id: str) -> Optional[TailFacts]:
        row = next((r for r in rows if r.row_id == row_id), None)
        age_s = resolve_session_truth(row.name).get("last_activity_age_s") if row else None
        epoch = now_s - float(age_s) if age_s is not None else None
        return TailFacts((), epoch, "", None, None, ())

    claim_for = claim_for or (lambda node: _answered(_claim_view(node)))
    if node_state_for is None:
        index = _graph_index()
        node_state_for = lambda node: _answered(index).get(node)  # noqa: E731
    vs = verdicts(rows, transcript_for=transcript_for, claim_for=claim_for,
                  node_state_for=node_state_for, now_s=now_s, silence_after_s=silence_after_s)
    return vs, rows


#: claude's INPUT spellings folded onto this lane's vocabulary. Derived from
#: the harness's own map rather than re-enumerated: ``busy`` was added here by
#: hand once and its sibling ``needs input`` was missed, so a row wearing the
#: second spelling of blocked could never ghost, reroute or wake. One source
#: means the next spelling claude adds cannot be missed the same way.
_CANONICAL_STATE = {"Working": "working", "Needs input": "blocked",
                    "Idle": "idle", "Done": "done"}

#: Tail readings that mean the session is still IN PLAY, so a
#: finished-with-the-tree read on one is wrong. ``watching`` is a worker
#: parked by the loop runtime, ``your-move`` is one holding a question for a
#: human, ``working`` is one whose silence has not yet reached the stalled
#: threshold.
_ENGAGED_TAILS = frozenset({"watching", "your-move", "working"})

#: States that mean the roster considers a session over. The classifier
#: asks the transcript, never this: the roster called a working session done
#: on 2026-08-15. The one use left is deciding whether an unmapped spelling
#: deserves a drift warning, where a terminal word is expected and anything
#: else is news.
_TERMINAL_STATES = frozenset({"stopped", "done", "completed", "exited", "killed"})

def _row_state(r: dict) -> tuple[str, str]:
    """This lane's canonical state for a row, and a warning when it is new.
    Reads ``("state", "status")`` in that order (``_STATUS_KEYS`` in
    harnesses/claude.py), folds spellings through the harness map, and
    returns an unknown spelling as-is WITH a warning: "" or a silent fall
    out of every lane set is the all-clear this lane exists to refuse."""
    from fno.agents.harnesses.claude import _LIVE_STATUS_INPUT

    for key in ("state", "status"):
        value = r.get(key)
        if isinstance(value, str) and value.strip():
            raw = value.strip().lower()
            mapped = _LIVE_STATUS_INPUT.get(raw)
            if mapped is None:
                if raw in _TERMINAL_STATES:
                    return raw, ""
                return raw, (
                    f"{ADVISORY_WARNING_PREFIX}unmapped row state {raw!r}, "
                    "classified by name only"
                )
            return _CANONICAL_STATE.get(mapped, mapped.lower()), ""
    return "", (
        f"{ADVISORY_WARNING_PREFIX}row carried no state under either alias, "
        "unmeasurable"
    )


def _is_linked_worktree(cwd: str) -> bool:
    """Is ``cwd`` a linked git worktree (its own checkout), not a shared one?

    A linked worktree's ``.git`` is a FILE holding a gitdir pointer; the
    canonical checkout's is a DIRECTORY. Only in the former is the
    ``.fno/target-state.md`` manifest one node for one session."""
    if not cwd:
        return False
    try:
        return (Path(cwd) / ".git").is_file()
    except OSError:
        return False


class _Unreadable:
    """A seam read that FAILED, which is not the fact an empty answer states.

    Both seams answered ``{}`` on any exception, so an unreadable claims root
    and a node with no claim produced one value - the two facts a caller must
    be able to tell apart (an unreadable read is never a verdict).
    """

    __slots__ = ("detail",)

    def __init__(self, detail: str) -> None:
        self.detail = detail


def _answered(value: Any) -> Any:
    """Unfold a failed seam read into the raise its caller treats as unknown."""
    if isinstance(value, _Unreadable):
        raise RuntimeError(value.detail)
    return value


def _claim_view(node: str) -> dict | _Unreadable:
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    key = f"node:{node}"
    try:
        return claim_status(key, root=claims_root_for(key))
    except Exception as exc:  # noqa: BLE001 - an unreadable claim condemns nothing
        return _Unreadable(f"claims root unreadable ({exc!r})")


def _graph_index() -> dict[str, dict] | _Unreadable:
    from fno.graph.load import load_graph

    try:
        entries = load_graph()
    except Exception as exc:  # noqa: BLE001 - a graph miss is never node state
        return _Unreadable(f"graph unreadable ({exc!r})")
    return {
        str(e.get("id")): e for e in entries if isinstance(e, dict) and e.get("id")
    }


#: A sweep over ZERO rows is an unreadable instrument, never an empty fleet
#: (king report 2026-08-17, node x-4c87: after a binary update the roster read
#: 0 registered rows against an intact 19-row registry file). A zero-row sweep
#: would write ``counts={}`` and a fresh sweep-file mtime, which reads as a
#: healthy quiet fleet - an empty fleet and a broken instrument must never
#: produce the same output. The same absence-as-evidence rule as the wake lane.
ROSTER_REFUSAL = (
    "roster unreadable: 0 rows (an empty fleet and a broken instrument "
    "read the same; refusing so staleness reads loud)"
)


def measure_provider_outages(
    rows: list[Row], *, now_s: float,
    settings: Any = None,
    entries_provider: Optional[Callable[[], list[Any]]] = None,
    transcript_path_for: Optional[Callable[[Any], Path | None]] = None,
    pane_read_fn: Optional[Callable[[str, Any], str]] = None,
    pane_snapshot_dir: Optional[Path] = None,
    journal: Optional[Path] = None,
    entries_for: Optional[Callable[[str], Optional[list[dict]]]] = None,
) -> dict[str, Any]:
    """Collect durable transcript records, then persist exact pane fallbacks.

    ``entries_for(row_id)`` supplies the tick's shared transcript parse (see
    :func:`tail_entries`); with it, no transcript file is opened here."""
    from fno.agents.provider_outage import (
        EvidenceIdentity,
        OutagePolicy,
        collect_pane_evidence,
        collect_transcript_evidence,
        journal_path as provider_journal_path,
        measure_and_persist,
        pane_read_via_mux,
    )

    if entries_provider is None:
        from fno.agents.registry import load_registry

        entries_provider = load_registry
    try:
        entries = entries_provider()
    except Exception as exc:  # noqa: BLE001 - an unreadable join refuses action
        report = _unknown_provider_report("provider_identity_registry_unreadable")
        report["refusals"][0]["detail"] = repr(exc)
        return report
    by_session = {
        str(getattr(entry, "harness_session_id", "") or ""): entry
        for entry in entries
        if getattr(entry, "harness_session_id", None)
    }
    identities = []
    mux_by_row: dict[str, dict[str, Any]] = {}
    for row in rows:
        entry = by_session.get(row.row_id)
        identities.append(EvidenceIdentity(
            row_id=row.row_id,
            harness=str(getattr(entry, "harness", "") or ""),
            provider=getattr(entry, "route_provider_id", None),
            account=getattr(entry, "account_record_id", None),
            session_id=str(getattr(entry, "harness_session_id", "") or row.row_id),
            cwd=str(getattr(entry, "cwd", "") or row.cwd),
        ))
        mux = getattr(entry, "mux", None)
        if isinstance(mux, dict):
            mux_by_row[row.row_id] = mux
    try:
        if settings is None:
            from fno.config import load_settings

            settings = load_settings()
        policy = OutagePolicy.from_settings(settings)
    except Exception:  # noqa: BLE001 - use the schema floor on a config miss
        policy = OutagePolicy()
    records, refusals = collect_transcript_evidence(
        identities,
        now_s=now_s,
        transcript_path_for=transcript_path_for,
        evidence_freshness_s=policy.evidence_freshness_s,
        entries_for=entries_for,
    )
    # Rows whose transcript cannot be read fall back to the pane buffer: both
    # a MISSING transcript and an UNSUPPORTED transcript SHAPE leave the pane
    # as the only instrument that can see a 429/529. Without the second
    # reason in this set, every codex or opencode pane row reads as a named
    # refusal and the quorum never hears it - the exact shape that made the
    # non-Claude fleet invisible.
    unreadable_rows = {
        str(item.get("row_id"))
        for item in refusals
        if item.get("reason") in ("transcript_unreadable", "transcript_shape_unsupported")
    }
    if unreadable_rows:
        if pane_read_fn is None:
            pane_read_fn = pane_read_via_mux

        target = journal or provider_journal_path()
        snapshot_root = pane_snapshot_dir or target.parent / "provider-pane-snapshots"
        fallback_identities = [
            identity for identity in identities if identity.row_id in unreadable_rows
        ]
        pane_records, pane_refusals = collect_pane_evidence(
            fallback_identities,
            mux_by_row=mux_by_row,
            now_s=now_s,
            snapshot_dir=Path(snapshot_root),
            pane_read_fn=pane_read_fn,
        )
        successful_rows = {record.row_id for record in pane_records}
        refusals = [
            item for item in refusals
            if not (
                item.get("reason") in ("transcript_unreadable", "transcript_shape_unsupported")
                and str(item.get("row_id")) in successful_rows
            )
        ]
        records.extend(pane_records)
        refusals.extend(pane_refusals)
    if not records and refusals:
        counts = Counter(str(item["reason"]) for item in refusals)
        return {
            "instrument": "unknown",
            "breakers": [],
            "counts": dict(counts),
            "refusals": refusals,
        }
    report = measure_and_persist(
        records, now_s=now_s, path=journal, policy=policy
    )
    if refusals:
        report["refusals"] = [*report.get("refusals", []), *refusals]
        counts = Counter(str(item["reason"]) for item in refusals)
        for reason, count in counts.items():
            report.setdefault("counts", {})[reason] = count
    return report


def supervise_provider_handoffs(
    provider_outages: dict[str, Any], rows: list[Row], *, settings: Any,
    now_s: float,
    candidate_for: Optional[Callable[[dict[str, Any], Row, float], Any]] = None,
    handoff_fn: Optional[Callable[..., Any]] = None,
    deps_factory: Optional[Callable[[], Any]] = None,
    decision_fn: Optional[Callable[..., Any]] = None,
    journal_root: Optional[Path] = None,
) -> list[dict[str, Any]]:
    """Run at most one proved cross-provider transaction per source row."""
    if not handoff_armed(settings):
        return []
    instrument = provider_outages.get("instrument")
    if instrument != "measured":
        # Armed but blind: a config that wants handoff got no evidence to act
        # on this tick, for reasons that can be transient (a lock timeout, a
        # torn journal write) as easily as durable. Returning [] here reads
        # identically to "measured, nothing broken" - one named outcome tells
        # the difference apart instead of both going quiet the same way.
        return [{
            "phase": "refused",
            "reason": "provider_outage_instrument_unmeasured",
            "detail": f"instrument={instrument!r}; no handoff evidence this tick",
            "count": 1,
        }]
    from fno.agents.outage_handoff import (
        HandoffRequest,
        production_handoff_dependencies,
    )
    from fno.agents.provider_outage import OutagePolicy, select_healthy_destination
    from fno.recovery import recover_provider_outage

    if candidate_for is None:
        candidate_for = production_handoff_candidate
    handoff_fn = handoff_fn or (
        lambda request, deps, journal_root: recover_provider_outage(
            request, deps=deps, journal_root=journal_root, settings=settings
        )
    )
    deps_factory = deps_factory or production_handoff_dependencies
    if decision_fn is None:
        from fno.decide import record_decision

        decision_fn = record_decision
    root = journal_root or (sweep_path().parent / "recovery" / "transactions")
    policy = OutagePolicy.from_settings(settings)
    by_id = {row.row_id: row for row in rows}
    outcomes: list[dict[str, Any]] = []
    for breaker in provider_outages.get("breakers") or []:
        if not isinstance(breaker, dict):
            continue
        broken_provider = str(breaker.get("provider") or "")
        for row_id in breaker.get("row_ids") or []:
            row = by_id.get(str(row_id))
            if row is None or not row.node:
                outcomes.append({
                    "phase": "refused", "reason": "unknown_source_node",
                    "source_row_id": str(row_id), "count": 1,
                    "outage_epoch": str(breaker.get("outage_epoch") or ""),
                    "provider": broken_provider,
                    "account": str(breaker.get("account") or ""),
                })
                continue
            try:
                candidate = (
                    production_handoff_candidate(
                        breaker, row, now_s, settings=settings
                    )
                    if candidate_for is production_handoff_candidate
                    else candidate_for(breaker, row, now_s)
                )
                selected = select_healthy_destination(
                    [candidate] if candidate is not None else [],
                    broken_provider=broken_provider,
                    now_s=now_s,
                    policy=policy,
                )
                if selected is None or not selected.model:
                    outcomes.append({
                        "phase": "refused", "reason": "no_fresh_destination_canary",
                        "source_row_id": row.row_id, "node": row.node, "count": 1,
                        "outage_epoch": str(breaker.get("outage_epoch") or ""),
                        "provider": broken_provider,
                        "account": str(breaker.get("account") or ""),
                    })
                    continue
                request = HandoffRequest(
                    node=row.node,
                    outage_epoch=str(breaker.get("outage_epoch") or ""),
                    source_row_id=row.row_id,
                    destination_harness=str(selected.harness),
                    destination_provider=str(selected.provider),
                    destination_model=selected.model,
                    destination_account=str(selected.record_id),
                    source_provider=broken_provider,
                    source_account=str(breaker.get("account") or ""),
                    evidence_fingerprints=tuple(
                        str(item) for item in (breaker.get("fingerprints") or [])
                    ),
                    destination_account_env=selected.account_env or {},
                    quorum_evidence_count=len(breaker.get("fingerprints") or []),
                )
                result = handoff_fn(
                    request, deps=deps_factory(), journal_root=Path(root)
                )
                outcome = result.to_dict()
                outcome.update({
                    "provider": selected.provider,
                    "account": selected.record_id,
                    "source_provider": broken_provider,
                    "source_account": str(breaker.get("account") or ""),
                    "count": max(1, sum(result.counts.values())),
                })
                outcomes.append(outcome)
                contended = (
                    result.failed_phase == "observed"
                    and result.counts.get("lease_contention", 0) > 0
                )
                if (
                    result.phase in {"committed", "parked"}
                    and not result.replayed
                    and not contended
                ):
                    decision_fn(
                        subject=row.node,
                        decision=(
                            f"provider outage handoff {result.phase} for "
                            f"{broken_provider} to {selected.provider}"
                        ),
                        decided_by="provider-outage-supervisor",
                        authority_source="daemon-automation",
                        rationale=result.reason or "terminal provider-outage transaction",
                        source="daemon",
                    )
            except Exception as exc:  # noqa: BLE001 - one row's crash must not stall its siblings
                outcomes.append({
                    "phase": "refused", "reason": "handoff_supervision_crashed",
                    "source_row_id": row.row_id, "node": row.node, "count": 1,
                    "outage_epoch": str(breaker.get("outage_epoch") or ""),
                    "provider": broken_provider,
                    "account": str(breaker.get("account") or ""),
                    "detail": f"{type(exc).__name__}: {exc}",
                })
    return outcomes


def production_handoff_candidate(
    breaker: dict[str, Any], row: Row, now_s: float, *,
    settings: Any = None,
    entries_provider: Optional[Callable[[], list[Any]]] = None,
    route_policy_provider: Optional[Callable[[Row], tuple[list[str], dict[str, str]]]] = None,
    account_env_for: Optional[Callable[[str, Path], dict[str, str]]] = None,
    route_env_for: Optional[Callable[[Any], dict[str, str]]] = None,
    runtime_exhausted_fn: Optional[Callable[[str, Path], bool]] = None,
    harness_installed_fn: Optional[Callable[[str], bool]] = None,
    pane_occupancy_fn: Optional[Callable[[str], int]] = None,
    canary_fn: Optional[Callable[[Any, Row, float], Any]] = None,
    open_breakers_provider: Optional[Callable[[], list[dict[str, Any]]]] = None,
    configured_routes_provider: Optional[Callable[[str, list[str]], list[Any]]] = None,
):
    """Walk configured route policy and return the first proved destination."""
    from fno.agents.provider_outage import (
        CanaryProof,
        HEALTH_MARKER,
        OutagePolicy,
        RouteCandidate,
        run_health_canary,
    )
    policy = OutagePolicy.from_settings(settings) if settings is not None else OutagePolicy()

    # Candidate route discovery is intentionally conservative: only a route
    # already stamped on a registry row is eligible. Missing explicit model,
    # provider, or account identity refuses instead of deriving one axis from
    # the harness or model label.
    try:
        from fno.adapters.providers.dispatch import dispatch_env
        from fno.agents.model_routing import read_route_settings
        from fno.agents.registry import load_registry

        entries_provider = entries_provider or load_registry
        route_policy_provider = route_policy_provider or _production_route_policy
        account_env_for = account_env_for or (
            lambda account, root: dispatch_env(account, repo_root=root)
        )
        route_env_for = route_env_for or (
            lambda entry: read_route_settings(entry.route_settings_path)
            if getattr(entry, "route_settings_path", None) else {}
        )
        runtime_exhausted_fn = runtime_exhausted_fn or _runtime_exhausted
        harness_installed_fn = harness_installed_fn or (
            lambda harness: shutil.which(harness) is not None
        )
        pane_occupancy_fn = pane_occupancy_fn or _production_pane_occupancy
        open_breakers_provider = open_breakers_provider or _persisted_open_breakers
        ordered_accounts, pins = route_policy_provider(row)
        broken_provider = str(breaker.get("provider") or "")
        broken_account = str(breaker.get("account") or "")
        if any(str(value) in {broken_provider, broken_account} for value in pins.values()):
            return None
        if configured_routes_provider is None:
            from fno.agents.autonomous_route import configured_outage_routes

            def configured_routes_provider(cwd: str, ordered: list[str]) -> list[Any]:
                return configured_outage_routes(cwd, ordered_record_ids=ordered)
        configured_by_account = {
            route.record_id: route
            for route in configured_routes_provider(row.cwd, ordered_accounts)
        }
        entries_by_account: dict[str, list[Any]] = {}
        for registered in entries_provider():
            account = str(getattr(registered, "account_record_id", "") or "")
            if account:
                entries_by_account.setdefault(account, []).append(registered)
        open_routes = {
            (str(item.get("provider") or ""), str(item.get("account") or ""))
            for item in open_breakers_provider()
            if isinstance(item, dict)
        }

        for account in ordered_accounts:
            configured = configured_by_account.get(account)
            if configured is not None:
                harness = str(configured.harness)
                provider = str(configured.provider)
                model = str(configured.model)
                account_env = dict(configured.account_env)
                route_env = dict(configured.route_env)
            else:
                # A live row may supplement a legacy configured record that
                # predates explicit model axes, but it is never the candidate
                # denominator: only ids from ordered_accounts reach this loop.
                observations = entries_by_account.get(account, [])
                identities = {
                    (
                        str(getattr(item, "harness", "") or ""),
                        str(getattr(item, "route_provider_id", "") or ""),
                        str(getattr(item, "model_name", "") or ""),
                    )
                    for item in observations
                }
                if len(identities) != 1:
                    continue
                harness, provider, model = next(iter(identities))
                if not all((harness, provider, model)):
                    continue
                entry = observations[0]
                root = Path(row.cwd)
                account_env = account_env_for(account, root)
                route_env = route_env_for(entry)
            pin_provider = str(pins.get("provider") or "")
            if pin_provider and pin_provider not in {account, harness, provider}:
                continue
            if pins.get("harness") and str(pins["harness"]) != harness:
                continue
            if pins.get("model") and str(pins["model"]) != model:
                continue
            root = Path(row.cwd)
            candidate = RouteCandidate(
                record_id=account,
                harness=harness,
                provider=provider,
                model=model,
                account=account,
                account_env=account_env,
                route_env=route_env,
                canary=None,
                breaker_open=(provider, account) in open_routes,
                runtime_exhausted=runtime_exhausted_fn(account, root),
                harness_installed=harness_installed_fn(harness),
                pane_supported=harness in {"codex", "opencode", "agy"},
                pane_count=pane_occupancy_fn(harness),
            )
            if (
                provider == broken_provider
                or candidate.breaker_open
                or candidate.runtime_exhausted
                or not candidate.harness_installed
                or not candidate.pane_supported
                or candidate.pane_count >= 4
            ):
                continue

            if canary_fn is not None:
                proof = canary_fn(candidate, row, now_s)
                if proof is not None:
                    return RouteCandidate(**{**candidate.__dict__, "canary": proof})
                continue

            from fno import _subprocess_util, paths
            from fno.agents.mux_spawn import _mux_pane_alive

            canary_cwd = paths.state_dir() / "recovery" / "canary-work"
            canary_cwd.mkdir(parents=True, exist_ok=True)

            def collect(spawned: Any) -> CanaryProof | None:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    proc = subprocess.run(
                        [*_subprocess_util.fno_py_cmd(), "mux", "pane", "read",
                         "--session", spawned.session, str(spawned.pane_id),
                         "--lines", "20"],
                        capture_output=True, text=True, timeout=5, check=False,
                    )
                    lines = [line.strip() for line in proc.stdout.splitlines()]
                    if HEALTH_MARKER in lines:
                        observed = time.time()
                        proof_path = paths.state_dir() / "recovery" / "provider-canaries"
                        proof_path.mkdir(parents=True, exist_ok=True)
                        digest = hashlib.sha256(
                            f"{candidate.record_id}\0{spawned.pane_id}".encode()
                        ).hexdigest()[:20]
                        from fno.state.io import atomic_write

                        atomic_write(proof_path / f"{digest}.json", json.dumps({
                            "provider": candidate.provider,
                            "account": candidate.record_id,
                            "pane_id": str(spawned.pane_id),
                            "observed_at": observed,
                            "content": HEALTH_MARKER,
                        }, sort_keys=True))
                        return CanaryProof(
                            source="pane", content=HEALTH_MARKER,
                            observed_at=observed, persisted=True,
                            assistant_role=False, pane_id=str(spawned.pane_id),
                        )
                    time.sleep(0.25)
                return None

            def stop(spawned: Any) -> bool:
                subprocess.run(
                    [*_subprocess_util.fno_py_cmd(), "mux", "pane", "kill",
                     "--session", spawned.session, str(spawned.pane_id)],
                    capture_output=True, text=True, timeout=10, check=False,
                )
                stopped = _mux_pane_alive({
                    "session": spawned.session, "pane_id": spawned.pane_id,
                }) is False
                if stopped:
                    from fno.agents.registry import update_registry

                    update_registry(lambda entries: [
                        item for item in entries
                        if not (
                            item.name == spawned.name
                            and item.mux == {
                                "session": spawned.session,
                                "pane_id": spawned.pane_id,
                            }
                        )
                    ])
                return stopped

            proof = run_health_canary(
                candidate,
                canary_cwd=canary_cwd,
                node_cwd=Path(row.cwd),
                now_s=now_s,
                collect_proof=collect,
                stop=stop,
                policy=policy,
            )
            if proof is not None:
                return RouteCandidate(**{**candidate.__dict__, "canary": proof})
    except Exception as exc:  # noqa: BLE001 - a crash here must not masquerade as "no candidate"
        logging.getLogger(__name__).warning(
            "watchdog: candidate discovery crashed for %s: %s", row.row_id, exc
        )
        return None
    return None


def _production_route_policy(row: Row) -> tuple[list[str], dict[str, str]]:
    from fno.adapters.providers.loader import load_combos, load_providers
    from fno.agents.dispatch_target import resolve_dispatch_target

    root = Path(row.cwd)
    index = _graph_index()
    if isinstance(index, _Unreadable):
        index = {}
    node = index.get(str(row.node), {}) if row.node else {}
    pins = {
        key: str(node.get(key) or "")
        for key in ("provider", "harness", "model")
        if str(node.get(key) or "").strip()
    }
    target = resolve_dispatch_target(
        "provider-outage-handoff", repo_root=root, env={}
    )
    if target.provider_id:
        return [target.provider_id], pins
    if target.combo_name:
        combo = load_combos(repo_root=root).get(target.combo_name)
        return (list(combo.providers) if combo is not None else []), pins
    config = load_providers(repo_root=root)
    return ([config.active] if config.active else []), pins


def _runtime_exhausted(account: str, root: Path) -> bool:
    from fno.adapters.providers.loader import load_quota_config
    from fno.adapters.providers.runtime_state import HeadroomState, headroom

    quota = load_quota_config(repo_root=root)
    return headroom(
        account,
        ttl_seconds=quota.probe_ttl_seconds,
        threshold_pct=quota.defer_threshold_pct,
        repo_root=root,
    ).state is HeadroomState.EXHAUSTED


def _persisted_open_breakers() -> list[dict[str, Any]]:
    from fno.agents.provider_outage import journal_path

    try:
        value = json.loads(journal_path().read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return []
    breakers = value.get("breakers") if isinstance(value, dict) else None
    return [item for item in (breakers or []) if isinstance(item, dict)]


def _production_pr_state(cwd: str, pr_number: int) -> Optional[str]:
    """Current PR state via the routed reader in the row's own checkout;
    None is UNKNOWN and the polling lane stays silent on it."""
    try:
        proc = subprocess.run(
            [*_fno(), "do", "pr", "info", str(pr_number)],
            capture_output=True, text=True, timeout=15, check=False,
            cwd=cwd or None,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        state = json.loads(proc.stdout).get("state")
    except ValueError:
        return None
    return str(state) if state else None


def _production_pane_occupancy(harness: str) -> int:
    """Live pane count for ONE harness in the resolved mux session, joined
    off each pane's ``harness_session_id`` - never a cwd or title guess;
    panes with no harness session count toward nothing. An unreadable
    listing answers 4, the at-capacity value: the route gate skips a
    candidate it could not measure (fail-closed - counting an unreadable
    mux as empty overfills the session with recovery spawns).
    """
    from fno import _subprocess_util
    from fno.agents.mux_spawn import resolve_mux_session

    session = resolve_mux_session(None)
    proc = subprocess.run(
        [*_subprocess_util.fno_py_cmd(), "mux", "pane", "ls",
         "--session", session, "--json"],
        capture_output=True, text=True, timeout=10, check=False,
    )
    if proc.returncode != 0:
        return 4
    try:
        panes = json.loads(proc.stdout or "")
    except (TypeError, ValueError):
        return 4
    if not isinstance(panes, list):
        return 4
    session_ids = [
        sid
        for sid in (
            str(item.get("harness_session_id") or "")
            for item in panes
            if isinstance(item, dict)
        )
        if sid
    ]
    if not session_ids:
        return 0
    from fno.agents.registry import load_registry

    harness_by_sid: dict[str, str] = {}
    for entry in load_registry():
        sid = str(getattr(entry, "harness_session_id", "") or "")
        if sid:
            harness_by_sid[sid] = str(getattr(entry, "harness", "") or "")
    return sum(1 for sid in session_ids if harness_by_sid.get(sid) == harness)


def run_sweep(
    *,
    now_s: Optional[float] = None,
    rows_provider: Optional[Callable[[], tuple[list[Row], list[str]]]] = None,
    transcript_fn: Optional[Callable[[str], Optional[TailFacts]]] = None,
    claim_fn: Optional[Callable[[str], dict | _Unreadable]] = None,
    graph_fn: Optional[Callable[[], dict[str, dict] | _Unreadable]] = None,
    provider_outage_fn: Optional[Callable[[], dict[str, Any]]] = None,
    roster_timeout: Optional[float] = None,
    pr_state_fn: Optional[Callable[[str, int], Optional[str]]] = None,
) -> tuple[dict, list[Row]]:
    """Build the real seams and classify the whole fleet once. Returns
    ``(payload, rows)``, rows index-aligned with the verdicts so an apply
    lane can reach each row's cwd. Read-only - actions live in
    :func:`apply_verdict`. A zero-row enumeration sets ``payload["refused"]``
    and classifies nothing: callers must not write the sweep file or advance
    the mail gate on it, so the missing write turns into loud staleness
    instead of clean evidence."""
    now_s = now_s if now_s is not None else datetime.now(timezone.utc).timestamp()
    rows, warnings = (
        rows_provider() if rows_provider is not None
        else fleet_rows(timeout=roster_timeout)
    )
    # ONE transcript read per row per tick: the shared parse below feeds both
    # the outage evidence collector and the tail classifier. The defect this
    # replaces read and parsed every transcript twice per tick - once per
    # consumer - which was pure cost on the hottest path in the sweep.
    entries_by_row: dict[str, Optional[list[dict]]] = {}
    if provider_outage_fn is None and rows_provider is None:
        for row in rows:
            try:
                entries_by_row[row.row_id] = tail_entries(
                    row.row_id, row.cwd, agent=row.agent
                )
            except Exception:  # noqa: BLE001 - a failed read is never a verdict
                entries_by_row[row.row_id] = None
        provider_outages = measure_provider_outages(
            rows, now_s=now_s,
            entries_for=lambda row_id: entries_by_row.get(row_id),
        )
    elif provider_outage_fn is None:
        provider_outages = _unknown_provider_report("provider_outage_collector_missing")
    else:
        try:
            provider_outages = provider_outage_fn()
        except Exception as exc:  # noqa: BLE001 - unreadable evidence refuses action
            provider_outages = {
                "instrument": "unknown",
                "breakers": [],
                "counts": {"provider_outage_read_failed": 1},
                "refusals": [{"reason": "provider_outage_read_failed", "detail": repr(exc)}],
            }
    if not rows:
        return {
            "generated_at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            ),
            "verdicts": [],
            "counts": {},
            "warnings": [*warnings, ROSTER_REFUSAL],
            "refused": ROSTER_REFUSAL,
            "provider_outages": provider_outages,
        }, rows
    cwd_by_sid = {r.row_id: r.cwd for r in rows}
    agent_by_sid = {r.row_id: r.agent for r in rows}
    if transcript_fn is None:
        def transcript_fn(sid: str) -> Optional[TailFacts]:
            if sid in entries_by_row:
                return _facts_from_entries(entries_by_row[sid], _TAIL_RECORDS)
            return tail_facts(
                sid, cwd_by_sid.get(sid, ""), agent=agent_by_sid.get(sid, "claude")
            )
    claim_fn = claim_fn or _claim_view
    if graph_fn is None:
        index = _graph_index()

        def graph_fn() -> dict[str, dict] | _Unreadable:
            return index

    # Passing the sentinel through instead would read as "no claim" and "no
    # node state", which is exactly what the swallowed exception used to say.
    def claim_for(node: str) -> dict:
        return _answered(claim_fn(node))

    def node_state_for(node: str) -> Optional[dict]:
        return _answered(graph_fn()).get(node)

    # ONE graph read serves every row, so one failed read turns the whole fleet
    # STALE. The per-row verdicts stay honest; this names the single cause once
    # instead of leaving it to be inferred from N identical bases.
    graph_state = graph_fn()
    if isinstance(graph_state, _Unreadable):
        warnings = [*warnings, f"graph unreadable for every row: {graph_state.detail}"]
    # The PR-state reader only arms on the fully-production path: a test that
    # forgets a PR stub gets a silent lane, never a subprocess against its
    # fixtures. Memoized per distinct (cwd, PR) so rows share one read.
    base_pr_state = pr_state_fn
    if base_pr_state is None and rows_provider is None and transcript_fn is None:
        base_pr_state = _production_pr_state
    seen_pr_states: dict[tuple[str, int], Optional[str]] = {}

    def pr_state_for(cwd: str, pr_number: int) -> Optional[str]:
        if (cwd, pr_number) not in seen_pr_states and base_pr_state is not None:
            seen_pr_states[(cwd, pr_number)] = base_pr_state(cwd, pr_number)
        return seen_pr_states.get((cwd, pr_number))

    vs = verdicts(
        rows,
        transcript_for=transcript_fn,
        claim_for=claim_for,
        node_state_for=node_state_for,
        now_s=now_s,
        provider_outages=provider_outages,
        pr_state_for=pr_state_for,
    )
    counts: dict[str, int] = {}
    for v in vs:
        counts[v.verdict] = counts.get(v.verdict, 0) + 1
    payload = {
        "generated_at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "verdicts": [v._asdict() for v in vs],
        "counts": counts,
        "terminal_harness_rows": sum(r.state in _TERMINAL_STATES for r in rows),
        "warnings": warnings,
        "provider_outages": provider_outages,
    }
    return payload, rows


def emit_event(kind: str, data: dict) -> None:
    """Best-effort schema-validated event on the global events.jsonl. The
    source is ``daemon`` for both cadences (the envelope enum's only allowed
    word; distinguishing tick from hand-run belongs in a schema change). The
    swallow is LOUD: a miss that means "this lane writes nothing at all" must
    not look like silence, and telemetry never breaks a sweep.
    """
    try:
        from fno import paths
        from fno.events import _build, append_event

        append_event(
            _build(kind, "daemon", data), paths.state_dir() / "events.jsonl"
        )
    except Exception as exc:  # noqa: BLE001 - telemetry must never break the sweep
        logging.getLogger(__name__).warning(
            "watchdog: event %s not written: %s", kind, exc
        )


def sweep_path() -> Path:
    from fno import paths

    return paths.state_dir() / "watchdog-sweep.json"


def write_sweep_file(
    source: str,
    counts: Optional[dict],
    now_s: float,
    signature: Optional[str] = None,
    *,
    events_signature: Optional[str] = None,
    terminal_harness_rows: Optional[int] = None,
    recoverable_count: Optional[int] = None,
    unfinished: Optional[dict] = None,
    recovery_events_signature: Optional[str] = None,
    provider_outages: Optional[dict[str, Any]] = None,
) -> None:
    """Freshness evidence for the done probe: one small state file per sweep,
    best-effort (an unwritable state root must never break a tick). The
    ``signature`` of the non-leave verdict set rides along so the mail and
    event lanes can skip what says exactly what the last sweep said;
    ``unfinished`` carries the unfinished-work report stamps, and
    ``unfinished_work_complete`` is True only when all four dimensions were
    measured, so an incomplete read never certifies itself fresh."""
    try:
        path = sweep_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            previous = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, UnicodeDecodeError):
            previous = {}
        # The cadence stamp survives a hand-run. One file serves both
        # cadences, so a manual write used to erase the only evidence the
        # TICK had run, and `pr-watch status` then read a healthy cadence as
        # "FLEET WATCHDOG STALE, last sweep 0m old" - a line that blames the
        # daemon for the operator having looked.
        last_tick = now_s if source == "tick" else _last_tick_epoch()
        # The verdict lane's top-level stamps (counts, signature,
        # events_signature, terminal_harness_rows) belong to the session
        # verdict cadence. A REPORT write must not speak for that lane: it
        # carries the previous values through (None means carry), exactly
        # the protection the tick cadence stamp already gets.
        if counts is None:
            counts = previous.get("counts") or {}
        if signature is None:
            signature = str(previous.get("signature") or "")
        if events_signature is None:
            events_signature = str(previous.get("events_signature") or "")
        if terminal_harness_rows is None:
            terminal_harness_rows = int(previous.get("terminal_harness_rows") or 0)
        if provider_outages is None:
            # Same carry rule as counts and signature: a write from another
            # lane (the unfinished-work report, a recovery stamp) must not
            # speak for the provider fold. A MISSING measurement from this
            # lane's own run stamps the named-unknown report instead.
            provider_outages = previous.get("provider_outages") or _unknown_provider_report(
                "provider_outage_report_missing"
            )
        payload: dict[str, Any] = {
            "source": source,
            "at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            ),
            "counts": counts,
            "terminal_harness_rows": int(terminal_harness_rows),
            "signature": signature,
            "events_signature": events_signature,
            "provider_outages": provider_outages,
        }
        if recoverable_count is not None:
            payload["recoverable_count"] = int(recoverable_count)
        elif isinstance(previous.get("recoverable_count"), int):
            payload["recoverable_count"] = previous["recoverable_count"]
        if unfinished is not None:
            payload["unfinished_counts"] = dict(unfinished.get("counts") or {})
            payload["unfinished_work_complete"] = bool(unfinished.get("complete"))
            payload["unfinished_signature"] = str(unfinished.get("signature") or "")
            payload["unfinished_events_signature"] = str(
                unfinished.get("events_signature") or ""
            )
        else:
            # An apply/diagnostic run must not erase the report's evidence
            # any more than a hand-run may erase the tick's.
            for key in (
                "unfinished_counts",
                "unfinished_work_complete",
                "unfinished_signature",
                "unfinished_events_signature",
            ):
                if key in previous:
                    payload[key] = previous[key]
        if recovery_events_signature is not None:
            payload["recovery_events_signature"] = recovery_events_signature
        elif isinstance(previous.get("recovery_events_signature"), str):
            payload["recovery_events_signature"] = previous[
                "recovery_events_signature"
            ]
        if last_tick is not None:
            payload["last_tick_epoch"] = last_tick
        path.write_text(json.dumps(payload), encoding="utf-8")
    except OSError:
        pass


def _last_unfinished_events_signature() -> str:
    """The report EVENT lane's gate. Keyed on the finding-set identity and
    advanced by every publish, independent of the mail stamp: with no mail
    recipient configured the mail gate legitimately never advances, and an
    event gate chained to it would re-emit every finding every tick."""
    try:
        return str(
            json.loads(sweep_path().read_text(encoding="utf-8")).get(
                "unfinished_events_signature"
            )
            or ""
        )
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return ""


def _last_recovery_events_signature() -> str:
    try:
        return str(
            json.loads(sweep_path().read_text(encoding="utf-8")).get(
                "recovery_events_signature"
            )
            or ""
        )
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return ""


def _last_unfinished_signature() -> str:
    try:
        return str(
            json.loads(sweep_path().read_text(encoding="utf-8")).get(
                "unfinished_signature"
            )
            or ""
        )
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return ""


def _last_signature() -> str:
    try:
        return str(json.loads(sweep_path().read_text(encoding="utf-8")).get("signature") or "")
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return ""


def _last_events_signature() -> str:
    try:
        return str(
            json.loads(sweep_path().read_text(encoding="utf-8"))
            .get("events_signature")
            or ""
        )
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return ""


def fresh_non_leave(payload: dict, prev_events_signature: str) -> set:
    """Row ids whose non-leave verdict the EVENT lane has not already
    published: k stuck rows on a 600s tick append thousands of events a day
    saying one thing, long after the mail lane's change gate went quiet."""
    prev = set(filter(None, prev_events_signature.split(";")))
    return {
        v["row_id"]
        for v in payload["verdicts"]
        if v["verdict"] != LEAVE
        and f"{v['row_id']}:{v['verdict']}" not in prev
    }


#: The pr_watch launchd cadence, in seconds. A sweep older than two intervals
#: means the cadence is dead, and a dead cadence is indistinguishable from a
#: healthy fleet unless the staleness itself is published (the king's required
#: ship condition: absence is never evidence, so status reads loud, not clean).
SWEEP_INTERVAL_S = 600
SWEEP_STALE_AFTER_S = 2 * SWEEP_INTERVAL_S


def _last_tick_epoch() -> Optional[float]:
    """The epoch of the last TICK-sourced sweep, carried across hand-runs."""
    try:
        data = json.loads(sweep_path().read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return None
    value = data.get("last_tick_epoch")
    if isinstance(value, (int, float)):
        return float(value)
    # A file written before this field existed still proves a tick ran.
    return None


def sweep_staleness(
    now_s: Optional[float] = None, *, stale_after_s: float = SWEEP_STALE_AFTER_S
) -> dict:
    """``{"age_s", "stale", "source", "at"}`` for the last sweep, or
    ``{"stale": True, "age_s": None, ...}`` when no sweep ever ran. Never
    raises; a missing file is the loudest case, not a clean one."""
    now_s = now_s if now_s is not None else datetime.now(timezone.utc).timestamp()
    try:
        stat = sweep_path().stat()
        data = json.loads(sweep_path().read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {"age_s": None, "stale": True, "source": None, "at": None}
    source = str(data.get("source") or "")
    # A hand-run refreshes the file but proves nothing about the launchd
    # cadence, and the cadence is what this read exists to measure. So the
    # age is measured from the last TICK, not from the file: otherwise
    # running the sweep by hand both hides a dead cadence (fresh mtime) and,
    # once that was fixed by requiring source == "tick", reported a healthy
    # one as stale. Neither reading was about the thing being asked.
    tick_epoch = data.get("last_tick_epoch")
    if isinstance(tick_epoch, (int, float)):
        age = max(0.0, now_s - float(tick_epoch))
    elif source == "tick":
        age = max(0.0, now_s - stat.st_mtime)
    else:
        # No cadence evidence at all: a hand-run is the only sweep on record.
        return {
            "age_s": int(max(0.0, now_s - stat.st_mtime)),
            "stale": True,
            "source": source,
            "at": str(data.get("at") or ""),
        }
    return {
        "age_s": int(age),
        "stale": age > stale_after_s,
        "source": source,
        "at": str(data.get("at") or ""),
    }


def verdict_signature(payload: dict) -> str:
    """Stable identity of the non-leave verdict set (row_id:verdict, sorted).
    Two sweeps that agree on the fleet produce one signature, so the mail lane
    speaks only on change - a row stuck for a day reads once, not 72 times."""
    parts = sorted(
        f"{v['row_id']}:{v['verdict']}"
        for v in payload["verdicts"]
        if v["verdict"] != LEAVE
    )
    terminal_rows = int(payload.get("terminal_harness_rows") or 0)
    if terminal_rows:
        parts.append(f"terminal-harness-rows:{terminal_rows}")
    for breaker in payload.get("provider_outages", {}).get("breakers", []):
        parts.append(
            "provider-breaker:"
            f"{breaker.get('provider')}:{breaker.get('account')}:"
            f"{breaker.get('outage_epoch')}"
        )
    return ";".join(parts)


def union_signature(*signatures: str) -> str:
    """Merge signatures into one, preserving the sorted-parts shape.

    A filtered hand-run publishes a SUBSET, so its stamp must say "these, and
    whatever was already said" - stamping the subset alone silently retracts
    every row it filtered out and the next tick re-emits them all.
    """
    parts: set[str] = set()
    for signature in signatures:
        parts.update(part for part in signature.split(";") if part)
    return ";".join(sorted(parts))


def digest_text(payload: dict, limit: int = 8) -> str:
    """One-screen digest of a sweep, basis riding along so the king can
    falsify each call. The verdict rows are LIST ITEMS, not bare lines: the
    mail style gate reads a bare line under a paragraph as an illegal
    mid-paragraph wrap (rule 6) - the first tick's digest was refused by
    exactly that, silently, and never delivered."""
    total = len(payload["verdicts"])
    counts = " ".join(f"{k}={v}" for k, v in sorted(payload["counts"].items()))
    terminal_rows = int(payload.get("terminal_harness_rows") or 0)
    lines = [
        f"fleet watchdog swept {total} rows. {counts} terminal harness rows: {terminal_rows}",
        "",
    ]
    non_leave = [x for x in payload["verdicts"] if x["verdict"] != LEAVE]
    for v in non_leave[:limit]:
        lines.append(f"- {v['verdict']} {v['name']}: {v['basis']}")
    more = len(non_leave) - limit
    if more > 0:
        lines.append(f"- {more} more row(s) not shown")
    return "\n".join(lines)


def _send_machine_report(
    to: str, body: str, *, runner: Callable | None = None
) -> tuple[bool, str]:
    """Send a generated watchdog report through the transport layer.

    The authored-prose gate belongs to operator-written relay mail. Watchdog
    reports use the existing dispatch transport directly, so their measured
    rows can exceed that prose budget without adding a caller-facing bypass.
    ``runner`` remains an injectable subprocess seam for the legacy unit tests.
    """
    if runner is not None:
        argv = [*_fno(), "agents", "mail", "send", "--origin", "recovery"]
        if to.startswith("project:"):
            argv += ["--to-project", to[len("project:"):]]
        else:
            argv.append(to)
        argv.append(body)
        try:
            proc = runner(argv, capture_output=True, text=True, timeout=60, check=False)
        except (OSError, subprocess.SubprocessError) as exc:
            return False, f"mail send failed: {exc}"
        return proc.returncode == 0, (proc.stdout or proc.stderr or "").strip()

    from fno.agents.dispatch import (
        DispatchAskError,
        dispatch_send,
        dispatch_send_to_project,
    )
    from fno.inbox.store import ProjectIdentificationError, resolve_project

    try:
        sender = resolve_project(cwd=Path.cwd(), flag_hint="--from-name")
    except ProjectIdentificationError:
        # A watchdog daemon can run outside any checkout. Let dispatch use its
        # existing neutral sender instead of dropping the alert at resolution.
        sender = None

    try:
        if to.startswith("project:"):
            if sender is None:
                result = dispatch_send_to_project(
                    to[len("project:"):],
                    body,
                    cwd=Path.cwd(),
                    budget_enforce=False,
                    origin="recovery",
                )
            else:
                result = dispatch_send_to_project(
                    to[len("project:"):],
                    body,
                    cwd=Path.cwd(),
                    from_name=sender,
                    budget_enforce=False,
                    origin="recovery",
                )
        else:
            if sender is None:
                result = dispatch_send(
                    name=to,
                    message=body,
                    provider=None,
                    cwd=Path.cwd(),
                    budget_enforce=False,
                    origin="recovery",
                )
            else:
                result = dispatch_send(
                    name=to,
                    message=body,
                    provider=None,
                    cwd=Path.cwd(),
                    from_name=sender,
                    budget_enforce=False,
                    origin="recovery",
                )
    except DispatchAskError as exc:
        return False, str(exc)

    if result.delivery == "hosted":
        return True, f"{result.msg_id} delivered (hosted)"
    return True, f"{result.msg_id} queued (durable) [{result.reason or 'live-miss'}]"


def mail_digest(
    payload: dict, to: str, *, runner: Callable | None = None
) -> tuple[bool, str]:
    """Push the verdict to a mail handle (push, not pull: a verdict the king
    has to remember to fetch goes unread). Skipped without comment when the
    non-leave set is unchanged since the last sweep. A ``project:<slug>``
    recipient addresses the project mailbox instead of one agent."""
    if not to:
        return False, "no recipient configured"
    if payload.get("refused"):
        # Never "all rows leave, nothing to say": zero rows read is not zero
        # rows found, and the digest must not claim either.
        return False, payload["refused"]
    non_leave = [v for v in payload["verdicts"] if v["verdict"] != LEAVE]
    if not non_leave and int(payload.get("terminal_harness_rows") or 0) == 0:
        return True, "all rows leave, nothing to say"
    if verdict_signature(payload) == _last_signature():
        return True, "unchanged since the last sweep, not mailed"
    return _send_machine_report(to, digest_text(payload), runner=runner)


def mail_gate(
    payload: dict, to: str, *, runner: Callable | None = None
) -> tuple[bool, str, str]:
    """``(ok, receipt, signature_to_stamp)``: run the mail lane and hand back
    the signature the sweep file should carry, so a caller cannot stamp a
    digest it never delivered. The stamp is the CURRENT signature only after
    a settled-ok mail (delivered / nothing to say / unchanged); a failed send
    or an empty recipient keeps the PREVIOUS stamp, leaving the gate armed
    against the last digest actually mailed so the next sweep retries instead
    of swallowing the verdict behind a signature it never sent."""
    if not to:
        return True, "no recipient", _last_signature()
    if payload.get("refused"):
        # A refused sweep keeps the PREVIOUS stamp: the gate stays armed
        # against the last digest actually mailed.
        return False, payload["refused"], _last_signature()
    ok, receipt = mail_digest(payload, to, runner=runner)
    stamp = verdict_signature(payload) if ok else _last_signature()
    return ok, receipt, stamp


def unfinished_mail_gate(
    snapshot, to: str, *, runner: Callable | None = None
) -> tuple[bool, str, str]:
    """``(ok, receipt, unfinished_signature_to_stamp)`` for the report path:
    the same push-not-pull change gate :func:`mail_gate` gives the verdict
    path, keyed on finding identity instead of session rows.

    An incomplete snapshot IS mailed. Withholding it read as "retry next
    sweep", but the common cause is a worktree root that was deleted, which
    fails the same way forever, so the lane went permanently mute while its
    findings kept printing to stdout under exit 0. The digest names every
    unread dimension and its warning, so the reader sees the shortfall.

    The change gate keys on finding identity alone, so a later COMPLETE scan
    finding exactly the same set sends nothing: the caveat is never retracted
    by mail. That is the cheaper half of the trade - keying completeness into
    the signature would re-mail an unchanged finding set on every flip."""
    from fno.agents.unfinished_work import snapshot_digest, snapshot_signature

    if not to:
        return True, "no recipient", _last_unfinished_signature()
    signature = snapshot_signature(snapshot)
    if not snapshot.findings:
        return True, "no findings, nothing to say", signature
    if signature == _last_unfinished_signature():
        return True, "unchanged since the last sweep, not mailed", signature
    ok, receipt = _send_machine_report(to, snapshot_digest(snapshot), runner=runner)
    stamp = signature if ok else _last_unfinished_signature()
    return ok, receipt, stamp


# ---------------------------------------------------------------------------
# Apply lanes (the watchdog owns the decision, never the mechanism)
# ---------------------------------------------------------------------------

def _fno() -> list[str]:
    from fno import _subprocess_util

    return [*_subprocess_util.fno_py_cmd()]


#: The shipped content-confirm cadence (dispatch._mux_content_confirm): resume
#: returns when the live STATE reads working, which is before the injected turn
#: is flushed to the transcript, so a one-shot read reports a landed wake as
#: refused.
_CONFIRM_ATTEMPTS = 40
_CONFIRM_INTERVAL_S = 0.25


def confirm_wake_landed(
    row_id: str,
    cwd: str,
    message: str,
    before_epoch: Optional[float],
    *,
    agent: str,
    attempts: Optional[int] = None,
    interval_s: Optional[float] = None,
    sleep: Callable[[float], None] = time.sleep,
) -> bool:
    """The message must appear in the recipient transcript AFTER the pre-wake
    marker, as a record whose whole text EQUALS the message (a substring match
    reads "Let me continue with the tests" as a landed wake). The state field
    is not evidence: ``wake.sh`` printed ``working -> working`` for both a
    message that landed and one that did not. The scan runs deeper than the
    classification tail so a chatty attach cannot push the message out."""
    # Read at call time, not as a def-time default, so the cadence stays one
    # knob a caller (and a test) can turn.
    tries = _CONFIRM_ATTEMPTS if attempts is None else attempts
    wait = _CONFIRM_INTERVAL_S if interval_s is None else interval_s
    for attempt in range(max(1, tries)):
        if attempt:
            sleep(wait)
        if _confirm_once(row_id, cwd, message, before_epoch, agent=agent):
            return True
    return False


def _confirm_once(
    row_id: str, cwd: str, message: str, before_epoch: Optional[float], *, agent: str
) -> bool:
    facts = tail_facts(row_id, cwd, agent=agent, max_records=_CONFIRM_RECORDS)
    if facts is None:
        return False
    for epoch, text in facts.records:
        if text != message:
            continue
        if before_epoch is None or (epoch is not None and epoch > before_epoch):
            # before_epoch None (no parseable pre-wake stamp) degrades to a
            # presence check: still content, never a state field. A record
            # with NO timestamp of its own never confirms - a torn old line
            # carrying the word is presence, not a landing.
            return True
    return False


#: Which verdicts each apply level may execute: ``wake`` cannot destroy work
#: so bare ``--apply`` stops there; reroute respawns, so it needs
#: ``--apply-all``. ghost NEVER auto-acts (the remedy is a respawn under a
#: new id, the operator's call).
LANES = {
    "wake": frozenset({WAKE, SILENCE}),
    "all": frozenset({WAKE, REROUTE, SANDBOX_BLOCKED, SILENCE}),
}

#: The one silent outcome: the verdict was outside the lane the caller asked
#: for, so nothing was attempted and there is nothing to report. Every other
#: outcome is news. See :func:`apply_verdict` for why surface is the default.
SKIPPED = "skipped"

#: The action HALF landed: the fleet changed and the verdict did not finish.
#: A caller must surface it, and it is never a refusal - `watchdog_refused`
#: reads "declined to act", so a rotated provider or a stopped session logged
#: under it is logged as if nothing happened.
PARTIAL = "partial"

#: Outcome word -> event name. Anything not named here is a true refusal:
#: nothing changed. Keep this the ONE fold, so a third emit site cannot
#: disagree with the first two about what an outcome means.
_OUTCOME_EVENTS = {"applied": "watchdog_applied", PARTIAL: "watchdog_partial"}


def outcome_event(outcome: str) -> str:
    """The events.jsonl type for an ``apply_verdict`` outcome word."""
    return _OUTCOME_EVENTS.get(outcome, "watchdog_refused")


class RotationBudget:
    """One global provider rotation per sweep: ``_default_failover`` mutates
    the ACTIVE provider and its storm cap is keyed per row, so N reroute
    rows would rotate N times and walk the account queue to queue-exhausted
    (the same one-shot guard ``fno.recovery.run_recovery_sweep`` holds)."""

    def __init__(self) -> None:
        self.rotated = False


def apply_verdict(
    v: Verdict,
    *,
    lanes: str,
    cwd: str = "",
    agent: Optional[str] = None,
    runner=subprocess.run,
    failover_fn: Optional[Callable[[Any, Any], str]] = None,
    rotation: Optional[RotationBudget] = None,
) -> tuple[str, str]:
    """Execute one verdict inside ``lanes`` ("wake" | "all"); only ``SKIPPED`` is
    silent. wake/silence resume with ``cwd`` set; the transcript reads run under
    the row's own harness (``agent or v.agent``); reroute uses recovery._redispatch."""
    if v.verdict not in LANES.get(lanes, frozenset()):
        return SKIPPED, f"{v.verdict} outside {lanes} lane"
    try:
        if v.verdict in (WAKE, SILENCE):
            return _apply_wake(v, cwd=cwd, runner=runner, agent=agent or v.agent)
        if v.verdict == REROUTE:
            return _apply_reroute(
                v, cwd=cwd, failover_fn=failover_fn, rotation=rotation
            )
        if v.verdict == SANDBOX_BLOCKED:
            return _apply_sandbox_blocked(v, cwd=cwd, runner=runner)
    except (OSError, subprocess.SubprocessError) as exc:
        return "refused", f"{v.verdict} action failed: {exc}"
    return SKIPPED, f"{v.verdict} has no auto-action"


def _apply_sandbox_blocked(
    v: Verdict, *, cwd: str, runner: Callable
) -> tuple[str, str]:
    proc = runner(
        [
            *_fno(),
            "agents",
            "rm",
            v.row_id,
            "--force",
            "--audit-reason",
            "codex-sandbox-blocked",
        ],
        capture_output=True,
        text=True,
        timeout=180,
        check=False,
        cwd=cwd or None,
    )
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "").strip().splitlines()
        return "refused", f"reap exit {proc.returncode}: {tail[-1] if tail else ''}"
    return "applied", f"reaped {v.name}; Codex sandbox blocked Git writes"


def _apply_wake(v: Verdict, *, cwd: str, runner: Callable, agent: str) -> tuple[str, str]:
    before = tail_facts(v.row_id, cwd, agent=agent)
    before_epoch = before.last_event_epoch if before is not None else None
    proc = runner(
        [*_fno(), "agents", "resume", v.row_id, "--message", WAKE_MESSAGE],
        capture_output=True, text=True, timeout=180, check=False,
        cwd=cwd or None,
    )
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "").strip().splitlines()
        return "refused", f"resume exit {proc.returncode}: {tail[-1] if tail else ''}"
    if not confirm_wake_landed(v.row_id, cwd, WAKE_MESSAGE, before_epoch, agent=agent):
        return (
            "refused",
            f"resume reported success but {WAKE_MESSAGE!r} is not in the "
            f"transcript after the wake",
        )
    return "applied", f"woke {v.name}; message confirmed in transcript"


def _apply_reroute(
    v: Verdict,
    *,
    cwd: str,
    failover_fn: Optional[Callable[[Any, Any], str]],
    rotation: Optional[RotationBudget] = None,
) -> tuple[str, str]:
    """Reroute through the FULL failover, not a bare respawn: a bare
    ``_redispatch`` lands the replacement on the SAME capped account and
    loops. ``_default_failover`` rotates and redispatches as one unit; with
    no alternate armed it returns ``queue-exhausted`` and this lane refuses,
    naming it, rather than looping the fleet on the dead account."""
    from fno.recovery import Candidate, _default_failover, classify_session_error

    if rotation is not None and rotation.rotated:
        return (
            "held",
            f"reroute held: the active provider already rotated this sweep "
            f"({v.basis}). Re-run after the swap settles",
        )
    if not cwd:
        return "refused", "reroute refused: no recorded worktree to respawn into"
    if not _is_linked_worktree(cwd):
        # `_redispatch` re-derives the node from `.fno/target-state.md` under
        # this cwd - the shared-manifest read `fleet_rows` refuses to trust
        # for identity, because on a canonical checkout every session reads
        # the same node. Acting on it force-releases a claim a DIFFERENT live
        # session may hold and then spawns a duplicate /target onto it.
        return (
            "refused",
            f"reroute refused: {cwd} is not a linked worktree, so the node "
            f"it respawns is read from a manifest other sessions share. "
            f"Rotate the provider and respawn this row by hand",
        )
    facts = tail_facts(v.row_id, cwd, agent=v.agent)
    err = classify_session_error(facts.tail_text if facts is not None else "")
    if err is None or not getattr(err, "triggers_swap", False):
        return "refused", f"reroute refused: tail is not swap-class ({v.basis})"
    fn = failover_fn or _default_failover
    # The lifecycle address is the SESSION ID, not the display name: a
    # registry-less row's ``name`` is claude's friendly label with no registry
    # row behind it, so ``fno agents stop <name>`` cannot fall back to the
    # session store and the reroute ends rotated-no-worker.
    candidate = Candidate(
        short_id=v.row_id[:8], sock_path="", jobs_dir=None,
        cwd=cwd, name=v.row_id,
    )
    outcome = fn(candidate, err)
    if rotation is not None and outcome in (
        "swapped", "rotated-no-worker", "notified",
    ):
        # Every one of these rotated the global active provider, whether or
        # not a replacement started.
        rotation.rotated = True
    if outcome == "swapped":
        return "applied", f"failover swapped ({v.basis})"
    if outcome == "notified":
        # The revive path failed and only a human ping fired: nothing was
        # delivered, so this is never an applied. It is not a "reported"
        # either - that word is the lane skip, and callers drop it. The
        # provider HAS rotated, which a reader must see.
        return (
            PARTIAL,
            f"failover rotated, replacement not spawned, human notified ({v.basis})",
        )
    if outcome == "rotated-no-worker":
        # The receipt must not claim the session is untouched: on this path
        # the stop and the node-claim force-release may ALREADY have run
        # before the spawn failed. Name what is certain and what to check.
        # PARTIAL, not refused: the provider rotated, so the fleet changed.
        return (
            PARTIAL,
            f"failover rotated but no replacement spawned ({v.basis}). The "
            "old session may already be stopped and its claim force-released. "
            "Re-check the row before acting on it",
        )
    return (
        "refused",
        f"reroute refused: failover outcome {outcome!r}, no alternate armed "
        f"({v.basis}). Nothing rotated and the session is left as-is",
    )
