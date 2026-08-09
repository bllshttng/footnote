"""One reachability derivation with a declared basis, behind every agents surface.

Six surfaces used to answer "is this agent live" and they answered four
different questions, all rendering into the same word:

    list / truth   has it produced output recently          (transcript)
    status         what lifecycle state did we last WRITE   (stored enum)
    top            is there an OS process                   (process census)
    mail-inject    can I put text in front of it right now  (control socket)
    peek           nothing about liveness; it reads a file  (transcript CONTENT)

They disagreed because they measure different things, so collapsing them into
one word destroyed information rather than adding it. This module keeps the one
question a supervisor actually asks -- is the agent REACHABLE -- and makes every
surface report the BASIS it answered from, so a reader can tell which question
was answered.

Four rules, all load-bearing:

1. Positive evidence comes ONLY from transcript activity age. No other signal
   may raise a verdict toward ``reachable``. (The registry's own PID SEMANTICS
   rule, generalized from pid to every signal: a live process may still be
   unreachable, so process liveness can falsify and can never establish.)
2. Falsifiers are MONOTONE toward ``unreachable``. A falsifier may lower a
   verdict and may never raise one.
3. ``unknown`` is TERMINAL. No consumer may coerce it to either pole. Absence of
   evidence stays absence of evidence.
4. Basis and age are part of the VALUE. A bare ``live`` is unprintable.

Why silence is never ``unreachable``
------------------------------------
This registry lists REACHABLE agents; it is not a process table. "Orphaned"
means unreachable, not dead, so a row is never condemned for being quiet. A
transcript can only ever supply POSITIVE evidence of activity; its absence is
absence of evidence, not evidence of absence. So a silent row with no falsifier
available -- and 89 percent of rows carry no pid at all -- resolves ``unknown``
with its age attached, never ``unreachable``. Only an affirmative falsifier
condemns a row.

This is what makes the destructive rule un-rederivable rather than merely
remembered: absence of a pane, absence of a pid, and absence of recent output
all contribute exactly NOTHING here, so "no pane means safe to reap" cannot be
reconstructed by editing a threshold.

Why transcript age is necessary but never sufficient
----------------------------------------------------
It is the only surface that never lied (argv, pid, the daemon record, and
state.json were each caught lying about a live session in one evening; see
:mod:`fno.agents.session_truth`). But it has two limits, and both are why it is
the sole POSITIVE term rather than the whole answer:

* Resolution. The liveness axis is a low-pass filter with a two-hour window, so
  it cannot separate "dead 43 minutes" from "thinking for 43 seconds". Every
  false-live lives in that gap, which is why the age always rides along.
* It measures FILE WRITES, not conversation. A transcript can be touched by a
  stub write, a resume attempt, or a tool result with no live session behind it.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Optional

from fno.agents.session_truth import (
    STALLED_AFTER_S,
    _humanize_age,
    resolve_session_truth,
)

REACHABLE = "reachable"
UNREACHABLE = "unreachable"
UNKNOWN = "unknown"

#: Basis when the transcript supplied positive evidence of recent activity.
TRANSCRIPT = "transcript"
#: Basis when nothing at all resolved: no transcript, no falsifier, no signal.
NO_EVIDENCE = "no-evidence"
#: Basis when a transcript resolved but has gone quiet. NOT a death sentence.
SILENT = "silent"

#: The older wire vocabulary `--status` filters on and the Rust table renders.
#: Kept stable so this derivation stays additive for every existing consumer; the
#: verdict, its basis, and its age ride alongside in their own fields rather than
#: by redefining a word other code already parses.
#:
#: Note UNKNOWN, not UNREACHABLE, is where a quiet row lands: silence is absence
#: of evidence, and only an affirmative falsifier condemns a row. The visible
#: effect is that rows which used to read `orphaned` purely for being quiet now
#: read `unknown` with their age attached.
#:
#: Lives here rather than beside either caller because BOTH lanes of
#: ``fno agents list`` render into it -- registry rows and the discovered
#: live-sessions lane -- and a second copy is how the two lanes ended up
#: disagreeing inside one payload.
WIRE_STATUS = {
    REACHABLE: "live",
    UNREACHABLE: "orphaned",
    UNKNOWN: "unknown",
}

#: Truth states that are positive evidence of activity. ``done`` is deliberately
#: absent: a worker that emitted <promise> finished its MISSION, which says
#: nothing about whether it is still up, and conflating the two is how a
#: finished-but-live worker got reported unreachable.
_ACTIVE_STATES = frozenset({"working", "watching", "your-move"})


@dataclass(frozen=True)
class Reachability:
    """A verdict that carries the evidence it was reached from.

    The basis is not decoration. A bare ``live`` is exactly what let six
    surfaces disagree without anyone noticing which question each had answered.
    """

    verdict: str
    basis: str
    age_s: Optional[int]

    def render(self) -> str:
        if self.age_s is None:
            return f"{self.verdict} ({self.basis})"
        return f"{self.verdict} ({self.basis}, last activity {_humanize_age(self.age_s)} ago)"


def classify_reachability(
    *,
    truth_state: Optional[str],
    age_s: Optional[int],
    falsifier: Optional[str],
) -> Reachability:
    """Pure classifier. ``falsifier`` is a basis string, or None for "did not fire".

    A probe that could not read its evidence must arrive here as ``None``, the
    same as a probe that read it and found the agent healthy. That collapse is
    deliberate and it is the most dangerous line in this module: if an unreadable
    pid were allowed to falsify, every permission error would become a death
    sentence and the reaping hazard would return through the back door.
    """
    if falsifier is not None:
        return Reachability(UNREACHABLE, falsifier, age_s)
    if truth_state in _ACTIVE_STATES:
        return Reachability(REACHABLE, TRANSCRIPT, age_s)
    if truth_state is None or truth_state == "unknown":
        return Reachability(UNKNOWN, NO_EVIDENCE, age_s)
    # done / stalled: a transcript resolved, but it is not positive evidence of
    # reachability. Silence and mission-complete are both UNKNOWN, never
    # unreachable -- the age is what makes that actionable for the reader.
    return Reachability(UNKNOWN, SILENT, age_s)


def pid_falsifier(
    pid: Optional[int], pid_start_time: Optional[int] = None
) -> Optional[str]:
    """``"process-gone"`` when a recorded process is provably gone, else None.

    A row with NO pid is not a row with a dead process. 89 percent of registry
    rows carry no pid, so treating a missing one as death would condemn nearly
    the whole registry -- the destructive rule, rebuilt by accident. Absence of a
    pid is absence of evidence and returns None here.

    Unreadable liveness (psutil missing, AccessDenied on another uid's process)
    also returns None: only a confident "gone" falsifies.
    """
    if pid is None:
        return None
    from fno.agents.spawn_gate import _pid_alive

    try:
        alive = _pid_alive(pid, pid_start_time)
    except Exception:  # noqa: BLE001 -- a broken probe must never condemn a row
        return None
    return "process-gone" if alive is False else None


def reachability(
    handle: str,
    *,
    pid: Optional[int] = None,
    pid_start_time: Optional[int] = None,
    stalled_after_s: float = STALLED_AFTER_S,
    **resolve_kwargs: Any,
) -> Reachability:
    """Resolve ``handle`` to a reachability verdict with its basis. Never raises.

    Keyed on the registry HANDLE rather than a session id, because an attach can
    re-mint a session id while the handle stays put; a join key a re-attach can
    change is not a join key.
    """
    truth = resolve_session_truth(
        handle, stalled_after_s=stalled_after_s, **resolve_kwargs
    )
    return classify_reachability(
        truth_state=truth.get("state"),
        age_s=truth.get("last_activity_age_s"),
        falsifier=pid_falsifier(pid, pid_start_time),
    )
