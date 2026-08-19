"""fno.agents.watchdog - the external fleet watchdog (x-55c3).

Runs OUTSIDE every session (a manual verb, or a leg on the pr_watch tick) and
decides, per fleet row: wake it, reroute it, reap it, or leave it. The decision
is made from TRANSCRIPT truth keyed by session id; the two stores (the fno
registry, ``claude agents --json``) are hints. Measured 2026-08-15: 8 roster
rows claimed ``working`` while their transcripts had not moved in 30+ minutes,
``claude agents --json`` inverted live/dead on a capped lane, and a bulk reap
that trusted a session's ``done`` state as terminal killed live sessions (a
``done`` row means the turn finished and the session is RESUMABLE). The
transcript was right every single time.

The classifier is one pure function over injected inputs (no subprocess inside
it), so tests need no live fleet. Mechanisms delegate: the wake lane calls
``fno agents resume`` (x-c136) and then confirms the message by CONTENT in the
recipient transcript - a state field reading ``working`` was caught claiming a
wake that never landed. Reroute reuses ``fno.recovery._redispatch`` (stop
FIRST, then respawn; skipping the stop wakes a duplicate when the window
opens). Reap refuses on a dirty worktree and leans on ``claude rm``'s own
refusal rather than forcing past it.

Two traps a stranger inherits (both measured by hand on 2026-08-15):
  - node identity joins on the recorded ``node:<id>`` claim holder / worktree
    manifest, NEVER on a name regex: eight auto-named workers read as
    nobody-on-this-node and were nearly double-dispatched.
  - a wake is confirmed by transcript content, never by a state field.

A third, added 2026-08-19 (x-cd1e): the ``unclaimed`` verdict flags a live row
whose node carries no claim, and it is ADVISORY. The worker is fine; the record
is wrong. It never wakes, reroutes or reaps, and its own blind spot is the shape
that produced the defect - see ``_unclaimed_node_basis``.
"""
from __future__ import annotations

import json
import logging
import re
import subprocess
import time
from collections import Counter, namedtuple
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

# The shipped tail classifier is the POSITIVE resumability marker: its
# ``stalled`` verdict asserts the session went silent while still owing its
# next move, which is a fact about the tail rather than an absence in it.
from fno.agents.session_truth import classify_tail

Verdict = namedtuple("Verdict", "row_id name state verdict basis action")
Row = namedtuple("Row", "row_id name state node cwd")
#: ``records`` is [(epoch_s_or_None, text)] newest-last; ``tail_text`` is the
#: flattened join of those texts; ``last_role``/``last_text`` describe the LAST
#: record so the wake gate can run the shipped tail classifier (a POSITIVE
#: resumability marker - the absence of a 429 is not one); ``last_kind`` is
#: "tool" when the last event was a tool call, which reap must never fire on.
#: No transcript resolving -> None (ghost), which is a different fact from a
#: resolved-but-quiet transcript.
TailFacts = namedtuple(
    "TailFacts",
    "records last_event_epoch tail_text last_role last_text last_kind",
    defaults=(None, "", None),
)

GHOST = "ghost"
REAP = "reap"
REROUTE = "reroute"
WAKE = "wake"
STALE = "stale"
LEAVE = "leave"
#: Advisory only (x-cd1e): the worker is fine, the RECORD is wrong. Never a
#: wake, never a reroute, never a reap - the action lanes below switch on the
#: specific verdict, so this one cannot reach any of them. It replaces LEAVE so
#: the row surfaces in the digest, which is the whole point: nothing today
#: notices a live worker on a node no claim covers.
UNCLAIMED = "unclaimed"

#: States that make a transcript-less row a ghost: the row claims a live-ish
#: session whose recorded id resolves to nothing. A ``stopped`` row with no
#: transcript is not a ghost - stopped is already the operator's answer.
#: ``_row_state`` folds claude's ``busy`` onto ``working`` and its ``needs
#: input`` onto ``blocked`` before the classifier runs, so on rows built by
#: ``fleet_rows`` the fold is what keeps a ``busy`` ghost from reading as a
#: healthy leave - not the ``busy`` entry here, which cannot match. It stays
#: because ``verdicts`` is a pure function anyone can hand a raw row, and a
#: caller that skips the fold must not silently lose the ghost lane.
_GHOST_STATES = frozenset({"working", "busy", "blocked"})
#: Membership here is CANDIDACY, not a wake. The lane below wakes only on
#: ``classify_tail == "stalled"``, the tail asserting the session went silent
#: while still OWING its next move, so a row that is genuinely mid-task
#: cannot slip through whatever word the roster wears.
#:
#: That is why ``working`` belongs here, and leaving it out was the bug this
#: module was built to fix. A state word is what a session CLAIMS, and this
#: lane exists because that claim lies: on 2026-08-18 a row read live and
#: ``working`` while its last transcript message was an API error 56 minutes
#: old, and woken by hand it opened a PR fifteen minutes later. The eight
#: stale-``working`` rows named in the module docstring are that same
#: population. A word a dead session still wears is no reason to skip it.
#:
#: ``stopped`` belongs for the mirror reason, and reviewers keep asking why,
#: since the ghost lane calls stopped "the operator's answer". A worker an
#: operator stopped after its turn reads not-stalled and is left alone;
#: dropping it would delete the lane's population (a session that dies
#: mid-turn is exactly a stopped row) to re-guard what the stalled check
#: already guards.
_WAKE_STATES = frozenset({"working", "blocked", "stopped"})

#: The roster enumeration budget. ``claude agents --json --all`` is a
#: fleet-wide live-status probe, not a status line: measured at 3.4s /
#: 1.1s / 3.4s on a 43-row fleet, so the shared 3.0s interactive default
#: times out, returns zero rows, and trips ROSTER_REFUSAL on EVERY tick -
#: a watchdog that never sweeps while reporting itself merely stale. The
#: sweep runs on a tick with no human waiting, so it buys the whole fleet.
#: The 3.0s default was not theoretical: `fno-agents resume` printed
#: "timed out after 3.0s, falling back to registry-only view" on an
#: operator's own commands, repeatedly, before anyone read it as a defect.
ROSTER_TIMEOUT_S = 30.0

#: Fraction of the roster budget that may be spent before the sweep warns.
ROSTER_HEADROOM = 0.5

#: Hard age ceiling on the wake lane (king ruling 2026-08-17): a session
#: stopped for two months has a dead node, a stale branch, and a context that
#: describes a repository that has moved - waking it is not recovery. Past the
#: ceiling the row reads ``stale`` (a needs-human bucket) and NEVER reaches an
#: action lane: the 429 reset stamp carries no date, so on an old tail its
#: time-of-day reading is garbage, which also poisons reroute.
#: Twelve hours, not twenty-four: `parse_sgt_stamp` picks the nearest of
#: yesterday/today/tomorrow, so a stamp carrying no date is only unambiguous
#: for half a day. Measured: a reset that truly passed 13h ago parses as 11h
#: in the FUTURE and reads live. A ceiling above the parser's own resolution
#: hands the action lanes a window that opened half a day ago.
WAKE_MAX_AGE_S = 12 * 3600

#: Reap is the one verdict that must satisfy THREE signals (king ruling
#: 2026-08-17, the c696fddd case): the basis says the process is real, the
#: last event says what it is doing NOW, and the node says its OLD task
#: finished. A done node proves the old task ended and proves nothing about
#: whether the session was re-tasked since - an operator mail can hand a
#: worker new work after its PR merges. So a done-node row reaps only when
#: the transcript has gone QUIET (past recovery's idle threshold,
#: ``config.recovery.idle_threshold_seconds``; this constant is only the
#: fallback when the config will not read) and its last event was not a tool
#: call. A row executing a tool never reaps.
REAP_QUIET_AFTER_S = 900

# Reset stamps ride the provider error text in Singapore local time (UTC+8):
# "02:48:21 SGT" is 18:48:21Z. Two sessions launched at 18:45 and 18:46 took
# a 429 they would not have taken three minutes later, so waking inside a
# closed window costs a real turn - which is why an UNPARSEABLE stamp
# classifies leave, never wake.
_RESET_STAMP_RE = re.compile(r"(\d{1,2}):(\d{2}):(\d{2})\s*(?:SGT|UTC\+8)")
_SGT_OFFSET_S = 8 * 3600
_TAIL_BYTES = 64 * 1024
#: 60, not 15: a chatty attach (restore markers, compaction lines) must not
#: push a still-live 429 out of the window the wake gate reads - the burned
#: turn inside a closed window is the module's measured failure, and a
#: 5-hour usage limit outlives any 15-record tail.
_TAIL_RECORDS = 60
#: Confirmation scans deeper still, so a landed wake message is never read
#: as refused because the attach that followed it was chatty.
_CONFIRM_RECORDS = 120

#: The generated no-session holder form (target_cli._successor_claim_holder
#: and init-target-state.sh's claim_owner_id): ``<UTC stamp>-<pid junk>-<hex>``.
#: Such a holder is an operator/daemon context, not a fleet session, so it
#: never justifies reaping a row as "held by another session". A claude UUID
#: can never match: its first segment is 8 hex chars and this shape puts a
#: literal ``T`` at position 9.
_GENERATED_HOLDER_RE = re.compile(r"^\d{8}T\d{6}Z-")

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
    ``records`` (``[(epoch, text)]``, newest-last).

    A rate event is the NEWEST record the shipped classifier
    (:func:`fno.recovery.classify_session_error`) marks quota-swap-class -
    the sole integration point recovery itself uses, so the watchdog and
    the failover cannot disagree about what a rate event is. The classifier
    is never narrowed by an extra prefilter: a real quota body with no
    error-shaped words ("429 rate limit reached, retry after 8s") must still
    hold the window. The accepted cost runs the OTHER way: prose quoting
    the full vocabulary ("we are rate limited on the search API") can hold a
    window closed and withhold a wake - erring closed never burns a turn
    inside a live window, which is the measured failure. ``none``: no such
    record. ``live``/``passed``: its reset stamp sits in the future / past.
    ``unknown``: it carries no parseable stamp - fail safe, the caller must
    not wake on it, and an older record's stamp never stands in for the
    newest one's."""
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
    quiet_after_s: float = REAP_QUIET_AFTER_S,
) -> list[Verdict]:
    """One verdict per row, in table precedence (ghost > reap > reroute >
    wake > leave). Each basis string names the measurement that decided it, so
    a reader can falsify the call. ``claim_for(node)`` returns the
    ``node:<id>`` claim view (``{"state", "holder"}``); ``node_state_for``
    returns the graph entry (``{"status", ...}``) or None."""
    # Occupancy is read from the TRANSCRIPT, never from the roster state.
    # This module exists because both stores lie about liveness, and the
    # measured 2026-08-15 inversion had claude report `done` for a session
    # that was working. Keying occupancy on that field let a live row count
    # as zero, which handed its quiet sibling a reap on the tree the live
    # one was mid-task in - the same reading-about-one-thing-as-a-verdict-
    # about-another that the reap predicate below exists to end.
    #
    # So the tally asks the same question the predicate asks: is there a
    # POSITIVE marker that this row is finished with the tree? A transcript
    # that is fresh says occupied. A transcript that is missing, unreadable
    # or unparseable says UNKNOWN, and unknown counts as occupied, because
    # the cost of guessing wrong is somebody's uncommitted work.
    facts_by_row: dict[str, Optional[TailFacts]] = {}
    for row in rows:
        try:
            facts_by_row[row.row_id] = transcript_for(row.row_id)
        except Exception:  # noqa: BLE001 - a failed read is never a verdict
            facts_by_row[row.row_id] = None

    def _still_in_the_tree(row: Row) -> bool:
        return not finished_with_the_tree(
            facts_by_row.get(row.row_id), now_s, quiet_after_s
        )

    occupants = Counter(
        row.cwd for row in rows if row.cwd and _still_in_the_tree(row)
    )

    def _cotenants(row: Row) -> int:
        # Subtract this row only if it was counted, or the subtraction
        # cancels a sibling that WAS counted and the guard inverts.
        if not row.cwd:
            return 0
        mine = 1 if _still_in_the_tree(row) else 0
        return max(0, occupants[row.cwd] - mine)

    out: list[Verdict] = []
    for row in rows:
        verdict = _verdict_one(
            row,
            facts=facts_by_row.get(row.row_id),
            claim_for=claim_for,
            node_state_for=node_state_for,
            now_s=now_s,
            quiet_after_s=quiet_after_s,
            cotenants=_cotenants(row),
        )
        # The unclaimed advisory upgrades a LEAVE, and it is applied HERE
        # rather than at a leave return because there are four of them. Putting
        # it on one read as protection and left the common case - a healthy
        # working row whose tail is not stalled - silently uncovered, which is
        # the first pitfalls entry happening inside the fix for it. Caught by
        # its own test.
        #
        # Only LEAVE is upgraded. Every other verdict already says something
        # louder and more actionable, and burying it under a record-keeping
        # note would trade a real signal for an advisory one.
        if verdict.verdict == LEAVE:
            unclaimed_basis = _unclaimed_node_basis(row, claim_for)
            if unclaimed_basis:
                verdict = verdict._replace(
                    verdict=UNCLAIMED, basis=f"{verdict.basis}; {unclaimed_basis}"
                )
        out.append(verdict)
    return out


# ---------------------------------------------------------------------------
# The reap predicate (the one gate to the only destructive verdict)
# ---------------------------------------------------------------------------

#: Reap's three answers. UNKNOWN is the whole point of the tri-state: every
#: read that FAILED, and every reading this lane cannot interpret, lands here
#: instead of being folded into NO or, worse, into YES.
REAP_YES = "yes"
REAP_NO = "no"
REAP_UNKNOWN = "unknown"


def lane_armed(settings: Any) -> bool:
    """Will the tick actually sweep? One condition, every reader.

    The tick leg required three things and the two status readers checked
    one, so flipping the master panic switch left `pr-watch status` printing
    FLEET WATCHDOG STALE for a cadence that was off on purpose - a permanent
    alarm about a deliberate silence. A freshness reader that does not share
    the producer's own condition is measuring a different subsystem.
    """
    try:
        return bool(
            getattr(settings.recovery, "watchdog", "off") in ("report", "wake")
            and settings.recovery.enabled
            and settings.autonomy.enabled
        )
    except Exception:  # noqa: BLE001 - a partial settings stub is not armed
        return False


def finished_with_the_tree(
    facts: Optional[TailFacts], now_s: float, quiet_after_s: float
) -> bool:
    """Is this session done with its worktree? One question, one answer.

    The occupancy tally and the reap predicate both need this, and when they
    each derived it their own way they disagreed. Occupancy asked "did the
    transcript move recently"; the predicate asked "is the tail engaged". A
    session parked on ``<watching>`` is silent, so the first said gone while
    the second said present, and that disagreement handed a sibling a reap on
    the tree the parked session was sitting in.

    Two callers, one function, so they cannot drift apart again. False is the
    safe answer and every unreadable input returns it.
    """
    if facts is None or facts.last_event_epoch is None:
        return False
    age_s = max(0.0, now_s - facts.last_event_epoch)
    if age_s <= quiet_after_s:
        return False
    return classify_tail(
        facts.last_role, facts.last_text, age_s
    ) not in _ENGAGED_TAILS


def reap_decision(
    row: Row,
    *,
    facts: Optional[TailFacts],
    node_state_for: Callable[[str], Optional[dict]],
    claim_for: Callable[[str], dict],
    now_s: float,
    quiet_after_s: float,
    cotenants: int,
    window: str = "none",
) -> tuple[str, str]:
    """The ONLY path to a reap verdict. Returns ``(answer, basis)``.

    Eight review findings across three rounds were one defect wearing eight
    costumes: a reading about one thing treated as a verdict about another,
    and three of them turned an ABSENCE into a positive verdict. Fixing them
    where they were found converged on nothing, because the shape was the
    bug. So every question reap asks comes through here, and the rule is
    uniform: a reap needs a POSITIVE marker, and anything else - a read that
    raised, a read that returned nothing, a reading this code does not
    recognise - is UNKNOWN. UNKNOWN never reaps.

    That is what makes it converge. A new state spelling, a transcript that
    moved, a schema that raised, a store that is briefly unreadable: none of
    them can produce the marker, so none of them can reach the delete. The
    failure mode of a bug here is a row a human has to look at, which is the
    direction this lane is allowed to fail in.

    The positive signals a reap needs, all of them present: the DELIVERABLE
    is settled (the node is done, or another live session holds its claim),
    the worktree is this row's ALONE, no 429 window is open, and the
    transcript says the session DECLARED ITSELF FINISHED.

    That last one used to be silence past a 900s bar, which is the defect
    this whole predicate exists to end: silence is a reading about the last
    write, never a verdict about whether the work is over. A worker parked on
    ``<watching>`` is silent. A worker waiting out a rate limit is silent. A
    worker re-tasked and thinking is silent. So the destructive lane now asks
    ``classify_tail`` and refuses every reading that says the session is
    still IN PLAY: parked on ``<watching>``, holding a question the operator
    owes an answer to, or simply still working. What remains is ``done`` (it
    said so) and ``stalled`` (it died mid-turn and owes a move nobody is
    coming to make), which is the pair the deliverable ruling is about - a
    node whose PR merged reaps at any age.

    Refusing ``working`` also lifts the quiet bar to the stalled threshold as
    a side effect, which fixes the inversion where the DESTRUCTIVE lane
    accepted 900s of silence while the harmless wake lane demanded 7200s.
    """
    if not row.node:
        return REAP_NO, ""

    # Read one: the deliverable. A store that raises is unknown, never "not
    # done" - that is the absence-as-verdict move this predicate exists to
    # refuse.
    try:
        entry = node_state_for(row.node)
    except Exception as exc:  # noqa: BLE001 - a failed read is never a verdict
        return REAP_UNKNOWN, f"node {row.node} state unreadable ({exc!r})"

    reap_basis = ""
    if entry is not None and entry.get("status") == "done":
        reap_basis = f"node {row.node} done"
    else:
        try:
            claim = claim_for(row.node)
        except Exception as exc:  # noqa: BLE001 - same rule as the node read
            return REAP_UNKNOWN, f"claim on {row.node} unreadable ({exc!r})"
        holder_sid = _holder_session(claim.get("holder"))
        if (
            claim.get("state") == "live"
            and holder_sid
            and holder_sid != row.row_id
            and not _GENERATED_HOLDER_RE.match(holder_sid)
        ):
            reap_basis = f"claim held by {holder_sid}"

    if not reap_basis:
        # The deliverable is not settled. This one IS a read that answered.
        return REAP_NO, ""

    # Read two: occupancy. `rm` deletes the WORKTREE, and a linked worktree
    # proves its .git is a file, never that one session owns it. Two rows
    # were measured working one tree on one node.
    if cotenants:
        return REAP_UNKNOWN, (
            f"{reap_basis} but {cotenants} other session(s) share {row.cwd}, "
            f"never reaped on a shared worktree"
        )

    # Read three: the transcript. None has two causes - never written, and
    # could not be read - and this lane cannot tell them apart, so it treats
    # neither as evidence.
    if facts is None:
        return REAP_UNKNOWN, (
            f"{reap_basis} but no transcript to read, and an unreadable "
            f"transcript is not evidence of a finished session"
        )
    if facts.last_event_epoch is None:
        return REAP_UNKNOWN, (
            f"{reap_basis} but no parseable last event, so quiet is unproven"
        )

    # Read four: a session waiting out a rate limit is silent and is NOT
    # finished. Reap outranks reroute in the table, so without this the
    # destructive lane got first look at exactly the rows reroute exists for.
    if window == "live":
        return REAP_UNKNOWN, (
            f"{reap_basis} but a 429 window is open, so silence is the rate "
            f"limit, not a finished session"
        )

    age_s = max(0.0, now_s - facts.last_event_epoch)
    if facts.last_kind == "tool":
        return REAP_NO, (
            f"{reap_basis} but last event is a tool call, never reaped on "
            f"tool activity"
        )

    # Read five: is this session finished with the tree? The SAME call the
    # occupancy tally makes, deliberately, because when the two derived it
    # separately they disagreed about a parked session and the disagreement
    # deleted a worktree somebody was sitting in.
    truth = classify_tail(facts.last_role, facts.last_text, age_s)
    if not finished_with_the_tree(facts, now_s, quiet_after_s):
        return REAP_NO, (
            f"{reap_basis}, quiet {_mins(now_s, facts.last_event_epoch)}m, "
            f"but the tail reads {truth}, which is a session still in play"
        )
    return REAP_YES, (
        f"{reap_basis}, tail reads {truth}, quiet "
        f"{_mins(now_s, facts.last_event_epoch)}m"
    )


def _mins(now_s: float, epoch: Optional[float]) -> Optional[int]:
    if epoch is None:
        return None
    return max(0, int((now_s - epoch) / 60))


def _age_clause(now_s: float, epoch: Optional[float]) -> str:
    n = _mins(now_s, epoch)
    return f"{n}m" if n is not None else "unknown"


def _verdict_one(
    row: Row,
    *,
    facts: Optional[TailFacts],
    claim_for: Callable[[str], dict],
    node_state_for: Callable[[str], Optional[dict]],
    now_s: float,
    quiet_after_s: float = REAP_QUIET_AFTER_S,
    cotenants: int = 0,
) -> Verdict:

    # ghost: the row claims working/blocked but its recorded id resolves to no
    # transcript - a wake that failed to attach, fell back to spawning, minted
    # a new id, and left this row claiming a session that does not exist. It
    # outranks reap because a row with no transcript cannot be safely stopped.
    if facts is None and row.state in _GHOST_STATES:
        return Verdict(row.row_id, row.name, row.state, GHOST,
                       f"no transcript for {row.row_id}", "report")

    window, reset_epoch, stamp = ("none", None, "")
    if facts is not None:
        window, reset_epoch, stamp = rate_limit_window(facts.records, now_s)

    if row.node:
        answer, reap_basis = reap_decision(
            row,
            facts=facts,
            node_state_for=node_state_for,
            claim_for=claim_for,
            now_s=now_s,
            quiet_after_s=quiet_after_s,
            cotenants=cotenants,
            window=window,
        )
        if answer is REAP_YES:
            return Verdict(row.row_id, row.name, row.state, REAP,
                           reap_basis, "stop+rm")
        if answer is REAP_UNKNOWN:
            # Not "leave": leave says the row was read and is healthy. This
            # says the read did not answer, which is a different fact and a
            # human's to resolve.
            return Verdict(row.row_id, row.name, row.state, STALE,
                           reap_basis, "report")
        if reap_basis:
            return Verdict(row.row_id, row.name, row.state, LEAVE,
                           reap_basis, "none")

    # stale: the hard age ceiling, BEFORE the 429 window math - the reset
    # stamp carries no date, so on a tail older than the ceiling its
    # time-of-day reading is garbage and would poison reroute below.
    facts_age_s: Optional[float] = None
    if facts is not None and facts.last_event_epoch is not None:
        facts_age_s = max(0.0, now_s - facts.last_event_epoch)
    if row.state in _WAKE_STATES and facts_age_s is not None:
        if facts_age_s > WAKE_MAX_AGE_S:
            return Verdict(
                row.row_id, row.name, row.state, STALE,
                f"{row.state} {int(facts_age_s // 3600)}h old, past the "
                f"{int(WAKE_MAX_AGE_S // 3600)}h wake ceiling, needs a human",
                "report",
            )

    # reroute: blocked on a 429 whose window has NOT opened. Waking bounces
    # (proved twice by hand); the session must be stopped before the window
    # opens or it wakes into a duplicate.
    if (
        row.state == "blocked"
        and window == "live"
        and reset_epoch is not None
        # A tail with no parsed timestamp skips the age ceiling above, so
        # without this the one lane that stops and respawns a session acted
        # on a row of unknown age. The wake lane below already refuses this
        # exact input; the louder lane must not be the laxer one.
        and facts is not None
        and facts.last_event_epoch is not None
    ):
        reset_utc = datetime.fromtimestamp(reset_epoch, tz=timezone.utc)
        return Verdict(
            row.row_id, row.name, row.state, REROUTE,
            f"429 resets {reset_utc.strftime('%H:%M:%SZ')}, "
            f"{_mins(reset_epoch, now_s)}m out",
            "redispatch",
        )

    # wake: blocked or stopped, a transcript exists, and no live 429 window.
    # Every condition is POSITIVE evidence (king ruling 2026-08-17): an age
    # under the ceiling, a parseable last event, and a tail that asserts the
    # session went silent while still owing its next move (classify_tail
    # ``stalled``). "No 429 in tail" is an absence and never a wake reason; a
    # row with no parseable evidence must never reach an action lane.
    if row.state in _WAKE_STATES and facts is not None:
        if facts.last_event_epoch is None:
            return Verdict(row.row_id, row.name, row.state, LEAVE,
                           "no parseable transcript evidence, not wakeable",
                           "none")
        if window == "unknown":
            return Verdict(row.row_id, row.name, row.state, LEAVE,
                           f"429 present, reset window unknown "
                           f"({stamp or 'no stamp'})", "none")
        if window == "live" and reset_epoch is not None:
            reset_utc = datetime.fromtimestamp(reset_epoch, tz=timezone.utc)
            return Verdict(row.row_id, row.name, row.state, LEAVE,
                           f"429 resets {reset_utc.strftime('%H:%M:%SZ')}, "
                           f"window not open", "none")
        truth = classify_tail(facts.last_role, facts.last_text, facts_age_s)
        if truth != "stalled":
            return Verdict(row.row_id, row.name, row.state, LEAVE,
                           f"tail reads {truth}, session does not owe a move",
                           "none")
        clause = ("last 429 window passed" if window == "passed"
                  else "silent, no 429 in tail")
        return Verdict(row.row_id, row.name, row.state, WAKE,
                       f"{row.state} {_mins(now_s, facts.last_event_epoch)}m "
                       f"silent, {clause}", "resume")

    # leave: everything else, including every healthy injectable row - the
    # watchdog never competes with the normal inject path.
    # Never "reachable": nothing here probes reachability, and the eight
    # stale-`working` rows that motivated this module read exactly this line
    # while their transcripts had not moved in 30+ minutes. A basis states
    # what was measured, which is the age of the last write and the state the
    # roster claims - two facts that can disagree, and did.
    basis = (
        f"state {row.state}, last turn "
        f"{_age_clause(now_s, facts.last_event_epoch)} ago, no lane applies"
        if facts is not None
        else f"no transcript, state {row.state}"
    )
    return Verdict(row.row_id, row.name, row.state, LEAVE, basis, "none")


def _unclaimed_node_basis(row: Row, claim_for: Callable[[str], dict]) -> str:
    """Is this live row working a node that no claim covers?

    BLIND SPOT, stated here beside the two the module header already records.
    A row whose node did not resolve carries ``node=None`` and cannot be
    checked, and that is PRECISELY the shape of a worker that never ran
    ``fno target init``: ``Row.node`` comes from the worktree manifest and then
    the session-keyed ledger, and both are written downstream of init. So this
    catches the manifest-written-but-unclaimed case and nothing else. Reading it
    as "every unclaimed worker is flagged" would make it the same decorative
    guard this change exists to remove.

    Everything else here degrades to silence. An unresolved node, a claim read
    that raises, or any state that is not a plain ``free`` reports nothing: an
    advisory that fires on an unreadable store trains its reader to ignore it.
    """
    if not row.node:
        return ""
    try:
        claim = claim_for(row.node)
    except Exception:  # noqa: BLE001 - a failed read is never a finding
        return ""
    if claim.get("state") != "free":
        return ""
    return f"node {row.node} carries NO claim while this row is live"


def _holder_session(holder: Optional[str]) -> Optional[str]:
    """The canonical holder parser, so the holder vocabulary (claude
    ``target-session:<uuid>`` today, codex durable thread ids as they land)
    lives in one place; a foreign holder shape returns None and condemns
    nothing."""
    from fno.agents.truth_status import _session_from_holder

    return _session_from_holder(holder)


# ---------------------------------------------------------------------------
# Real I/O seams (every one injectable; the classifier above stays pure)
# ---------------------------------------------------------------------------

def tail_facts(
    session_id: str, cwd: str, *, max_records: int = _TAIL_RECORDS
) -> Optional[TailFacts]:
    """Resolve a session's transcript and tail-read it. Never raises.

    Reuses the provenance resolver (content-aware across every project dir -
    the dir name is the LAUNCH cwd, never derivable from a repo or worktree
    name). A missing transcript is None, which the classifier renders as a
    fact (ghost / unknown-age), never as fresh. A 64KB tail read costs ~0.9ms
    per row measured over 130 rows, so sweeping the fleet is cheap.
    """
    from fno.provenance.observed import resolve_transcript_path

    try:
        path = resolve_transcript_path("claude", session_id, cwd)
    except Exception:  # noqa: BLE001 - a broken resolver is "no transcript"
        return None
    if path is None:
        return None
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            fh.seek(max(0, size - _TAIL_BYTES))
            chunk = fh.read()
    except OSError:
        return None
    lines = chunk.decode("utf-8", "replace").splitlines()
    if size > _TAIL_BYTES and lines:
        lines = lines[1:]  # a mid-file seek lands inside a line; drop it
    entries: list[tuple[Optional[float], str, Optional[str], Optional[str]]] = []
    for line in lines:
        try:
            e = json.loads(line)
        except Exception:  # noqa: BLE001 - a torn/foreign line is not data
            continue
        epoch: Optional[float] = None
        ts = e.get("timestamp")
        if ts:
            try:
                epoch = datetime.fromisoformat(
                    str(ts).replace("Z", "+00:00")
                ).timestamp()
            except ValueError:
                epoch = None
        text = _record_text(e)
        msg = e.get("message")
        role = msg.get("role") if isinstance(msg, dict) else None
        kind = ("tool" if _has_tool_use(e) else "text") if role else None
        entries.append((epoch, text, str(role) if role else None, kind))
    # The window bounds EVERYTHING downstream, the (role, text, kind) triple
    # included: a triple read from a record older than max_records would pair
    # a stale text with the fresh age and window inputs it is classified
    # against.
    window = entries[-max_records:]
    records = [(epoch, text) for epoch, text, _role, _kind in window]
    last_epoch = next((t for t, _ in reversed(records) if t is not None), None)
    last_role: Optional[str] = None
    last_text = ""
    last_kind: Optional[str] = None
    for _epoch, text, role, kind in reversed(window):
        if role:
            # The LAST role-bearing record inside the window decides the tail
            # classifier's input; a trailing user turn clears stale assistant
            # signals.
            last_role, last_text, last_kind = role, text, kind
            break
    return TailFacts(
        records, last_epoch, " ".join(t for _, t in records),
        last_role, last_text, last_kind,
    )


def _has_tool_use(e: dict) -> bool:
    """Does this record carry tool activity - a call OR a result? A trailing
    tool_result is a round trip mid-flight (the result landed, the next
    assistant turn has not), which is a session WORKING exactly like a
    trailing tool_use (the c696fddd case: re-tasked after its PR merged),
    and reap must never fire on either."""
    msg = e.get("message")
    content = msg.get("content") if isinstance(msg, dict) else None
    if not isinstance(content, list):
        return False
    return any(
        isinstance(p, dict) and p.get("type") in ("tool_use", "tool_result")
        for p in content
    )


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


def _ledger_nodes() -> dict[str, str]:
    """``{claude session id -> node id}`` from the execution ledger.

    A worker that ran in the CANONICAL checkout has no worktree manifest of
    its own (king ruling 2026-08-17: its deliverable must still reap), and
    the operator fleet's ``t-`` shorthand names are ambiguous (the node's
    dash is stripped, so the slug boundary is unknowable - the name-join trap
    in its exact measured form). The ledger is machine-written recorded
    identity: each execution entry names its node in ``graph_node_id``, the
    documented join key, and the claude sessions that ran it. ``title`` is
    the free-text task input and joins nothing - keying on it was the
    name-join trap wearing a second coat. A miss degrades to no node, which
    condemns nothing."""
    from fno import paths

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
        node = str(e.get("graph_node_id") or "")
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
    registry for recorded identity. Node identity is the worktree MANIFEST
    first (runtime-recorded), then the execution LEDGER for rows with no
    manifest of their own - both machine-written. Never a name regex."""
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
            f"roster probe took {elapsed:.1f}s of its {budget:.0f}s budget "
            f"for {len(raw)} row(s); past the budget the sweep reads zero "
            f"rows and refuses. Raise ROSTER_TIMEOUT_S",
        ]
    by_sid: dict[str, Any] = {}
    try:
        for entry in load_registry():
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
        # The manifest is per-ROW identity only when the cwd is that row's own
        # linked worktree. On a shared checkout (worktree.policy = "never", or
        # any row launched in the canonical root) every session reads the SAME
        # graph_node_id, so a done node would make every quiet sibling reapable.
        # There the ledger's session-keyed join is the only honest answer, and
        # a miss leaves node None, which condemns nothing.
        state, state_warning = _row_state(r)
        if state_warning:
            unmapped_states.add(state_warning)
        node = _node_id_from_worktree(cwd) if _is_linked_worktree(cwd) else None
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
    if skipped_no_sid:
        warnings = [
            *warnings,
            f"{skipped_no_sid} row(s) carried no session id, unmeasurable, skipped",
        ]
    warnings = [*warnings, *sorted(unmapped_states)]
    return out, warnings


#: claude's INPUT spellings folded onto this lane's vocabulary. Derived from
#: the harness's own map rather than re-enumerated: ``busy`` was added here by
#: hand once and its sibling ``needs input`` was missed, so a row wearing the
#: second spelling of blocked could never ghost, reroute or wake. One source
#: means the next spelling claude adds cannot be missed the same way.
_CANONICAL_STATE = {"Working": "working", "Needs input": "blocked",
                    "Idle": "idle", "Done": "done"}

#: Tail readings that mean the session is still IN PLAY, so a reap on one is
#: a kill and not a cleanup. ``watching`` is a worker parked by the loop
#: runtime, ``your-move`` is one holding a question for a human, ``working``
#: is one whose silence has not yet reached the stalled threshold.
_ENGAGED_TAILS = frozenset({"watching", "your-move", "working"})

#: States that mean the roster considers a session over. Occupancy does NOT
#: use this: it asks the transcript through ``finished_with_the_tree``,
#: because the roster called a working session done on 2026-08-15 and keying
#: the tally on that field let a live row count as zero. The use left is
#: deciding whether an unmapped spelling deserves a drift warning, where a
#: terminal word is expected and anything else is news.
_TERMINAL_STATES = frozenset({"stopped", "done", "completed", "exited", "killed"})


def _row_state(r: dict) -> tuple[str, str]:
    """This lane's canonical state for a row, and a warning when it is new.

    Two failures live here. The sibling parser reads ``("state", "status")``
    in that order (``_STATUS_KEYS`` in harnesses/claude.py - preferring
    ``status`` once resolved a working session to Idle), so a raw
    ``r["state"]`` reads "" on every row under a rename, and "" is in no lane
    set: the whole fleet classifies ``leave`` behind a fresh sweep file and a
    clean status line, the silent all-clear this lane exists to refuse. And
    claude spells one state several ways, so the fold has to come from the
    harness map. A spelling neither knows returns as-is WITH a warning,
    because falling into no lane silently is that same all-clear."""
    from fno.agents.harnesses.claude import _LIVE_STATUS_INPUT

    for key in ("state", "status"):
        value = r.get(key)
        if isinstance(value, str) and value.strip():
            raw = value.strip().lower()
            mapped = _LIVE_STATUS_INPUT.get(raw)
            if mapped is None:
                if raw in _TERMINAL_STATES:
                    return raw, ""
                return raw, f"unmapped row state {raw!r}, classified by name only"
            return _CANONICAL_STATE.get(mapped, mapped.lower()), ""
    return "", "row carried no state under either alias, unmeasurable"


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


def _claim_view(node: str) -> dict:
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    key = f"node:{node}"
    try:
        return claim_status(key, root=claims_root_for(key))
    except Exception:  # noqa: BLE001 - an unreadable claim condemns nothing
        return {}


def _graph_index() -> dict[str, dict]:
    from fno.graph.load import load_graph

    try:
        entries = load_graph()
    except Exception:  # noqa: BLE001 - graph miss degrades to "no node state"
        return {}
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


def run_sweep(
    *,
    now_s: Optional[float] = None,
    rows_provider: Optional[Callable[[], tuple[list[Row], list[str]]]] = None,
    transcript_fn: Optional[Callable[[str], Optional[TailFacts]]] = None,
    claim_fn: Optional[Callable[[str], dict]] = None,
    graph_fn: Optional[Callable[[], dict[str, dict]]] = None,
    roster_timeout: Optional[float] = None,
) -> tuple[dict, list[Row]]:
    """Build the real seams and classify the whole fleet once. Returns
    ``(payload, rows)`` - the payload is the ``--json`` shape
    (``{"generated_at", "verdicts", "counts", "warnings"}``) and the rows ride
    along index-aligned with the verdicts so an apply lane can reach each
    row's cwd. Read-only - actions live in :func:`apply_verdict`.

    A zero-row enumeration sets ``payload["refused"]`` and classifies
    nothing: callers must not write the sweep file or advance the mail gate
    on it, so the missing write turns into loud staleness instead of clean
    evidence."""
    now_s = now_s if now_s is not None else datetime.now(timezone.utc).timestamp()
    rows, warnings = (
        rows_provider() if rows_provider is not None
        else fleet_rows(timeout=roster_timeout)
    )
    if not rows:
        return {
            "generated_at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            ),
            "verdicts": [],
            "counts": {},
            "warnings": [*warnings, ROSTER_REFUSAL],
            "refused": ROSTER_REFUSAL,
        }, rows
    cwd_by_sid = {r.row_id: r.cwd for r in rows}
    if transcript_fn is None:
        def transcript_fn(sid: str) -> Optional[TailFacts]:
            return tail_facts(sid, cwd_by_sid.get(sid, ""))
    claim_fn = claim_fn or _claim_view
    if graph_fn is None:
        index = _graph_index()

        def graph_fn() -> dict[str, dict]:
            return index
    try:
        from fno.config import load_settings

        quiet_after_s = float(load_settings().recovery.idle_threshold_seconds)
    except Exception:  # noqa: BLE001 - config miss falls back to the default
        quiet_after_s = REAP_QUIET_AFTER_S
    vs = verdicts(
        rows,
        transcript_for=transcript_fn,
        claim_for=claim_fn,
        node_state_for=lambda node: graph_fn().get(node),
        now_s=now_s,
        quiet_after_s=quiet_after_s,
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
    }
    return payload, rows


def emit_event(kind: str, data: dict) -> None:
    """Best-effort schema-validated event on the global events.jsonl (the same
    path the pr_watch tick writes). A miss is swallowed: telemetry never breaks
    a sweep.

    The source is ``daemon`` for both cadences because that is what the
    envelope enum and this type's own ``sources`` list allow. Passing a word
    outside them raised inside ``_build``, and the swallow below turned that
    into every manual-lane event silently never being written - a whole lane
    of telemetry gone with nothing said. Distinguishing tick from hand-run
    belongs in a schema change, not in a value the schema rejects; the sweep
    file already carries that distinction and staleness is read off it.

    So the swallow is loud now. It still cannot break a sweep, but a miss
    that means "this lane writes nothing at all" must not look like silence.
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
    counts: dict,
    now_s: float,
    signature: str = "",
    *,
    events_signature: str = "",
    terminal_harness_rows: int = 0,
) -> None:
    """Freshness evidence for the done probe: one small state file per sweep,
    best-effort (an unwritable state root must never break a tick). The
    ``signature`` of the non-leave verdict set rides along so the mail lane
    can skip a digest that says exactly what the last one said;
    ``events_signature`` is the same set as the EVENT lane last emitted, so
    the tick can suppress per-row events that say what the last tick already
    said (the mail lane speaks on change, and so must the event lane)."""
    try:
        path = sweep_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        # The cadence stamp survives a hand-run. One file serves both
        # cadences, so a manual write used to erase the only evidence the
        # TICK had run, and `pr-watch status` then read a healthy cadence as
        # "FLEET WATCHDOG STALE, last sweep 0m old" - a line that blames the
        # daemon for the operator having looked.
        last_tick = now_s if source == "tick" else _last_tick_epoch()
        payload: dict[str, Any] = {
            "source": source,
            "at": datetime.fromtimestamp(now_s, tz=timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            ),
            "counts": counts,
            "terminal_harness_rows": terminal_harness_rows,
            "signature": signature,
            "events_signature": events_signature,
        }
        if last_tick is not None:
            payload["last_tick_epoch"] = last_tick
        path.write_text(json.dumps(payload), encoding="utf-8")
    except OSError:
        pass


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
    """One-screen digest of a sweep, house-style (one physical line per
    paragraph). The basis rides along so the king can falsify each call.

    The verdict rows are LIST ITEMS, not bare lines: ``fno mail send`` runs the
    style gate on the body and a bare line under a paragraph reads as an
    illegal mid-paragraph wrap (rule 6) - the first tick's digest was refused
    by exactly that, silently, and never delivered. A list marker starts a new
    block, which is the legal shape for a table of rows."""
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


def mail_digest(
    payload: dict, to: str, *, runner: Callable = subprocess.run
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
    argv = [*_fno(), "mail", "send"]
    if to.startswith("project:"):
        argv += ["--to-project", to[len("project:"):]]
    else:
        argv.append(to)
    argv.append(digest_text(payload))
    try:
        proc = runner(argv, capture_output=True, text=True, timeout=60, check=False)
    except (OSError, subprocess.SubprocessError) as exc:
        return False, f"mail send failed: {exc}"
    ok = proc.returncode == 0
    return ok, (proc.stdout or proc.stderr or "").strip()


def mail_gate(
    payload: dict, to: str, *, runner: Callable = subprocess.run
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


# ---------------------------------------------------------------------------
# Apply lanes (the watchdog owns the decision, never the mechanism)
# ---------------------------------------------------------------------------

def _fno() -> list[str]:
    from fno import _subprocess_util

    return [*_subprocess_util.fno_py_cmd()]


def worktree_refusal(cwd: str) -> Optional[str]:
    """Why this worktree may NOT be reaped, or None when it is clean. Named so
    the refusal is actionable: unstaged changes and unpushed commits are work
    that exists nowhere else. A branch with no upstream reads as unpushed -
    a worktree whose commits are not on any remote is exactly that.

    ``claude rm`` refuses a dirty worktree too, and reap never passes
    ``--force``, so that guard still backstops this one. This check is a
    DELIBERATE duplicate for three reasons: it runs before the STOP, where
    claude's runs at the rm, by which point refusing leaves a session dead
    and its row kept; it names the reason in a dry-run receipt, so a reader
    sees why a row will not reap without executing anything; and it is the
    lane's own hook, where the co-tenancy check the harness cannot make (it
    sees one session, never the fleet) is the one that matters most."""
    if not cwd:
        return "no recorded cwd"
    try:
        dirty = subprocess.run(
            ["git", "-C", cwd, "status", "--porcelain"],
            capture_output=True, text=True, timeout=30, check=False,
        )
        if dirty.returncode != 0:
            # A failed status with empty stdout reads as clean unless the
            # exit is checked - an index.lock held by the very agent working
            # there must read as NOT-reapable, not as a clean tree.
            return f"git status failed in {cwd} (exit {dirty.returncode})"
        n_dirty = len([ln for ln in (dirty.stdout or "").splitlines() if ln.strip()])
        if n_dirty:
            return f"{n_dirty} uncommitted change(s) in {cwd}"
        unpushed = subprocess.run(
            ["git", "-C", cwd, "rev-list", "--count", "@{upstream}..HEAD"],
            capture_output=True, text=True, timeout=30, check=False,
        )
        if unpushed.returncode != 0:
            return f"no upstream to compare in {cwd}"
        n = (unpushed.stdout or "").strip()
        if n and n != "0":
            return f"{n} unpushed commit(s) in {cwd}"
    except (OSError, subprocess.SubprocessError) as exc:
        return f"worktree check failed: {exc}"
    return None


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
    attempts: Optional[int] = None,
    interval_s: Optional[float] = None,
    sleep: Callable[[float], None] = time.sleep,
) -> bool:
    """The message must appear in the recipient transcript AFTER the pre-wake
    marker, as a record whose whole text EQUALS the message - a substring
    match reads "Let me continue with the tests" as a landed wake. The state
    field is not evidence: ``wake.sh`` printed ``working -> working`` for both
    a message that landed and one that did not (the same content-not-state
    contract as mail_inject's confirm_content_after). The scan runs deeper
    than the classification tail so a chatty attach cannot push the message
    out of the confirmation window."""
    # Read at call time, not as a def-time default, so the cadence stays one
    # knob a caller (and a test) can turn.
    tries = _CONFIRM_ATTEMPTS if attempts is None else attempts
    wait = _CONFIRM_INTERVAL_S if interval_s is None else interval_s
    for attempt in range(max(1, tries)):
        if attempt:
            sleep(wait)
        if _confirm_once(row_id, cwd, message, before_epoch):
            return True
    return False


def _confirm_once(
    row_id: str, cwd: str, message: str, before_epoch: Optional[float]
) -> bool:
    facts = tail_facts(row_id, cwd, max_records=_CONFIRM_RECORDS)
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


#: Which verdicts each apply level may execute. ``wake`` is the one lane that
#: cannot destroy work, so bare ``--apply`` stops there; reap and reroute both
#: stop a session, so they need ``--apply-all``. ghost NEVER auto-acts: the
#: remedy is a respawn under a new id, which is the operator's call.
LANES = {"wake": frozenset({WAKE}), "all": frozenset({WAKE, REROUTE, REAP})}

#: The one silent outcome: the verdict was outside the lane the caller asked
#: for, so nothing was attempted and there is nothing to report. Every other
#: outcome is news. See :func:`apply_verdict` for why surface is the default.
SKIPPED = "skipped"


class RotationBudget:
    """One global provider rotation per sweep.

    ``_default_failover`` mutates the ACTIVE provider in settings.yaml, and
    its storm cap is keyed per row, so N reroute rows would rotate N times
    and walk the whole account queue to queue-exhausted in one invocation.
    The shipped recovery sweep guards this with the same one-shot flag
    (``fno.recovery.run_recovery_sweep``); the watchdog must not be the lane
    that re-opens it."""

    def __init__(self) -> None:
        self.rotated = False


def apply_verdict(
    v: Verdict,
    *,
    lanes: str,
    cwd: str = "",
    runner=subprocess.run,
    failover_fn: Optional[Callable[[Any, Any], str]] = None,
    rotation: Optional[RotationBudget] = None,
    reap_enabled: Optional[bool] = None,
) -> tuple[str, str]:
    """Execute one verdict inside ``lanes`` ("wake" | "all"). Returns
    ``(outcome, detail)``. Exactly ONE outcome is silent: ``SKIPPED``, which
    says the verdict was outside the lane the caller asked for, so nothing
    was attempted. Every other word is news and callers surface all of them.

    That inversion is deliberate. Callers used to enumerate which outcomes
    were worth printing, and three receipts were swallowed by that list in
    turn: a stop that landed without its rm, a reap withheld by the config
    freeze, and a reroute held because the provider had already rotated.
    Each was added to the list only after a review found it missing.
    Defaulting to surface means the next outcome added here cannot go silent
    by omission.
    Mechanisms delegate: resume (which verifies the state move and holds its
    own single-writer claim), recovery._redispatch for reroute, stop + rm for
    reap - rm is never forced, ``claude rm``'s own refusal on a dirty worktree
    is a safety feature this lane leans on rather than bypasses. Every
    delegated lifecycle command runs with ``cwd`` set to the row's worktree:
    a registry-less row from another project must resolve in its own project,
    not in whatever project launched the sweep."""
    if v.verdict not in LANES.get(lanes, frozenset()):
        return SKIPPED, f"{v.verdict} outside {lanes} lane"
    if v.verdict == REAP:
        # The freeze sits HERE, at the one funnel every lane and every
        # caller passes through, rather than in the CLI that happens to
        # expose --apply-all today. A guard on one of several reachable
        # paths reads as protection and ships with the others open.
        allowed = _reap_execution_enabled() if reap_enabled is None else reap_enabled
        if not allowed:
            return (
                "frozen",
                "reap classified but not executed: config.recovery."
                "watchdog_reap is false. Reap deletes the worktree and a "
                "wrong one is unrecoverable, so it ships off. Turn it on to "
                "execute, or stop and rm this row by hand",
            )
    try:
        if v.verdict == WAKE:
            return _apply_wake(v, cwd=cwd, runner=runner)
        if v.verdict == REROUTE:
            return _apply_reroute(
                v, cwd=cwd, failover_fn=failover_fn, rotation=rotation
            )
        if v.verdict == REAP:
            return _apply_reap(v, cwd=cwd, runner=runner)
    except (OSError, subprocess.SubprocessError) as exc:
        return "refused", f"{v.verdict} action failed: {exc}"
    return SKIPPED, f"{v.verdict} has no auto-action"


def _reap_execution_enabled() -> bool:
    """Is the reap lane armed? Fails CLOSED on any config trouble.

    A config that cannot be read is not permission. This is the one switch
    whose wrong answer deletes a worktree, so an unreadable or malformed
    settings file withholds the action rather than assuming the default that
    happens to be convenient.
    """
    try:
        from fno.config import load_settings

        return bool(getattr(load_settings().recovery, "watchdog_reap", False))
    except Exception:  # noqa: BLE001 - unreadable config is never permission
        return False


def _apply_wake(v: Verdict, *, cwd: str, runner: Callable) -> tuple[str, str]:
    before = tail_facts(v.row_id, cwd)
    before_epoch = before.last_event_epoch if before is not None else None
    proc = runner(
        [*_fno(), "agents", "resume", v.row_id, "--message", WAKE_MESSAGE],
        capture_output=True, text=True, timeout=180, check=False,
        cwd=cwd or None,
    )
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "").strip().splitlines()
        return "refused", f"resume exit {proc.returncode}: {tail[-1] if tail else ''}"
    if not confirm_wake_landed(v.row_id, cwd, WAKE_MESSAGE, before_epoch):
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
    """Reroute through the FULL failover, not a bare respawn.

    ``_redispatch`` alone has a precondition its own docstring names: the
    caller must already have swapped the active provider, or the replacement
    spawns onto the SAME capped account and 429s immediately - a stop/respawn
    loop at one real spawn per apply run. ``_default_failover`` does the
    rotation and the redispatch as one unit; with no alternate provider
    armed it returns ``queue-exhausted`` and this lane refuses, naming it,
    rather than looping the fleet on the dead account."""
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
        # session may hold and then spawns a duplicate /target onto it. The
        # reap lane guards this one function below; the lane that ships ON
        # was the one missing it.
        return (
            "refused",
            f"reroute refused: {cwd} is not a linked worktree, so the node "
            f"it respawns is read from a manifest other sessions share. "
            f"Rotate the provider and respawn this row by hand",
        )
    facts = tail_facts(v.row_id, cwd)
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
            "partial",
            f"failover rotated, replacement not spawned, human notified ({v.basis})",
        )
    if outcome == "rotated-no-worker":
        # The receipt must not claim the session is untouched: on this path
        # the stop and the node-claim force-release may ALREADY have run
        # before the spawn failed. Name what is certain and what to check.
        return (
            "refused",
            f"failover rotated but no replacement spawned ({v.basis}). The "
            "old session may already be stopped and its claim force-released. "
            "Re-check the row before acting on it",
        )
    return (
        "refused",
        f"reroute refused: failover outcome {outcome!r}, no alternate armed "
        f"({v.basis}). Nothing rotated and the session is left as-is",
    )


def _apply_reap(v: Verdict, *, cwd: str, runner: Callable) -> tuple[str, str]:
    refusal = worktree_refusal(cwd)
    if refusal:
        return "refused", f"reap refused: {refusal}"
    if not _is_linked_worktree(cwd):
        # `claude rm` is documented as removing "session record + worktree",
        # and the ledger join means cwd is routinely a repo ROOT: a
        # worktree.policy = "never" project, or a bg session started in the
        # canonical checkout. Whether that arm scopes its delete is an
        # external binary's undocumented behaviour, and the blast radius of
        # being wrong is the main checkout. This lane deletes only a tree it
        # can prove is disposable.
        return (
            "refused",
            f"reap refused: {cwd or 'no cwd'} is not a linked worktree, so "
            f"rm would act on a canonical checkout. Stop and remove this row "
            f"by hand",
        )
    stopped = runner(
        [*_fno(), "agents", "stop", v.row_id],
        capture_output=True, text=True, timeout=60, check=False,
        cwd=cwd or None,
    )
    if stopped.returncode != 0:
        # Exit 2 is the lifecycle verbs' "not found in registry". The roster
        # is `claude agents --json --all`, which enumerates store-only rows
        # the registry never recorded (24 of 43 on the measured fleet), so
        # this is a permanent scope mismatch and not a transient failure -
        # the receipt has to say so or a reader retries it forever.
        why = (
            "not in the registry, and reap resolves registry rows only"
            if stopped.returncode == 2
            else (stopped.stderr or "").strip()[:200]
        )
        return "refused", f"stop exit {stopped.returncode}: {why}"
    removed = runner(
        [*_fno(), "agents", "rm", v.row_id],
        capture_output=True, text=True, timeout=60, check=False,
        cwd=cwd or None,
    )
    if removed.returncode != 0:
        # The stop already landed, so "refused" ("declined to act") is a
        # receipt that lies about a session which is now dead. Same partial
        # application the reroute lane reports honestly.
        return (
            "partial",
            f"stopped, but rm exited {removed.returncode} so the registry row "
            f"remains ({v.basis}). The session is already stopped - remove "
            f"the row by hand, never re-run this as a stop: "
            f"{(removed.stderr or '').strip()[:200]}",
        )
    return "applied", f"stopped and removed ({v.basis})"
