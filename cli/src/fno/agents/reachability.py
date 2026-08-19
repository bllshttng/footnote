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

Note the asymmetry that keeps this honest. A row with no pane recorded, or a mux
that cannot answer, is an ABSENCE and condemns nothing. A mux affirmatively
reporting that a pane exited is EVIDENCE, and does condemn -- the retired rule
falsified on the first and this one falsifies only on the second. Which is also
why suppressing a falsifier is never the fix for a wrong falsifier: the answer
is to consult the right authority, not to stop asking.

Progress is a second axis, never a fourth reachability value
-------------------------------------------------------------
A worker taking its turn, a worker that parked after finishing, and a worker
alive but unable to think (handed a model its endpoint cannot serve) all
classify ``reachable`` here -- correctly, because all three ARE reachable.
:func:`classify_progress` answers the orthogonal question "is it advancing,
awaiting the operator, parked, or refused" in its own ``progress`` /
``progress_basis`` fields, mirroring this module's own verdict-plus-basis
shape rather than widening ``WIRE_STATUS`` to a fourth word.

A reading about one artifact is not a verdict about the agent
-----------------------------------------------------------------
A missing transcript file proves that a file is missing at that path.
Nothing else. Every probe in this module follows that rule for reachability
(:func:`pid_falsifier`, :func:`pane_falsifier` each return ``None`` -- not a
death verdict -- when their own evidence is absent or unreadable), and
:func:`classify_progress` follows it for progress: an unresolved or missing
transcript classifies ``unknown`` on both axes, never ``refused`` and never
``parked``. Only the classifiers in this module may answer either axis; a
reader that derives liveness from bare file existence elsewhere is rebuilding
the same mistake one layer over.

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
    STALE_ATTENTION_S,
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


#: Taking its own next turn (``truth_state in {working, watching}``).
ADVANCING = "advancing"
#: Will move once a human moves (``truth_state == "your-move"``).
AWAITING_OPERATOR = "awaiting-operator"
#: Alive, turn finished, owes nothing, still costs RSS.
PARKED = "parked"
#: Alive, reachable, and unable to think -- see :func:`classify_progress`.
REFUSED = "refused"
#: No evidence either way. Shares the spelling with ``UNKNOWN`` on the
#: reachability axis on purpose: both answer "we don't know", just about
#: different questions.
PROGRESS_UNKNOWN = "unknown"

TRANSCRIPT_TURN = "transcript-turn"
OPERATOR_TURN = "operator-turn"
PROMISE = "promise"
MODEL_REFUSED = "model-refused"


@dataclass(frozen=True)
class Progress:
    """A progress verdict beside :class:`Reachability`, never inside it.

    Widening ``WIRE_STATUS`` to a fourth value would make one word answer two
    questions at once -- the exact collapse specimen 4 rebuilt by accident.
    ``reachability`` stays "can I reach this process"; ``progress`` answers
    "is it taking its own next turn, waiting on a human, parked, or refused",
    a question reachability was never asked and cannot answer: a refused
    worker is fully reachable.
    """

    verdict: str
    basis: str


def classify_progress(
    *,
    truth_state: Optional[str],
    reachability: str,
    observed_model: Optional[dict],
    harness: Optional[str],
    route_settings_path: Optional[str],
    last_activity_age_s: Optional[int],
) -> Progress:
    """Pure classifier for the progress axis. Never raises.

    Precedence, in order: a falsified/unreachable row first (a gone process
    has no progress state, and inventing one for it is the collapse again);
    then the refusal predicate (Locked Decision 3 -- structural, never reads
    the transcript's prose, so a reworded refusal message cannot break it);
    then the truth-state arms plus the measured transcript age. The refusal
    test MUST run before the truth-state arms: a refused worker emits exactly one assistant message
    and stops, so the transcript tail reads ``working`` for two hours and
    ``stalled`` after that, and testing state first would report
    ``advancing`` for the exact row this axis exists to catch. A ``working`` or
    ``watching`` state is only advancing while its measured transcript age is
    inside :data:`STALE_ATTENTION_S`; an absent age is unreadable evidence and
    resolves to ``unknown``.
    """
    if reachability == UNREACHABLE:
        return Progress(PROGRESS_UNKNOWN, NO_EVIDENCE)
    if _is_refused(observed_model, harness, route_settings_path):
        return Progress(REFUSED, MODEL_REFUSED)
    if truth_state in ("working", "watching"):
        if last_activity_age_s is None:
            return Progress(PROGRESS_UNKNOWN, NO_EVIDENCE)
        if last_activity_age_s >= STALE_ATTENTION_S:
            return Progress(PROGRESS_UNKNOWN, SILENT)
        return Progress(ADVANCING, TRANSCRIPT_TURN)
    if truth_state == "your-move":
        return Progress(AWAITING_OPERATOR, OPERATOR_TURN)
    if truth_state == "done":
        return Progress(PARKED, PROMISE)
    if truth_state == "stalled":
        return Progress(PROGRESS_UNKNOWN, SILENT)
    return Progress(PROGRESS_UNKNOWN, NO_EVIDENCE)


def _is_refused(
    observed_model: Optional[dict],
    harness: Optional[str],
    route_settings_path: Optional[str],
) -> bool:
    """Structural refusal predicate (Locked Decision 3). Never reads prose.

    Fails OPEN by construction: a recorded ``route_settings_path`` records
    the INTENDED route (registry.py), so a foreign-routed worker answering
    as a foreign model is healthy, not refused. Missing a refusal is
    recoverable; condemning a healthy foreign-routed worker is the reaping
    hazard this module exists to prevent.
    """
    if harness != "claude":
        return False
    if route_settings_path is not None:
        return False
    if not observed_model or observed_model.get("kind") != "observed":
        return False
    model = observed_model.get("model")
    if not isinstance(model, str):
        return False
    from fno.agents.model_routing import is_anthropic_model

    return not is_anthropic_model(model)


def pid_falsifier(pid: Optional[int], pid_start_time: Optional[int] = None) -> Optional[str]:
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


def pane_falsifier(mux: Any) -> Optional[str]:
    """``"pane-gone"`` when the mux states the pane exited, else None.

    The tri-state matters: ``_mux_pane_alive`` answers True (up), False (exited)
    and None (the mux cannot answer). Only False is evidence. ``reconcile``
    records the None case as ``mux-pane-liveness-unavailable`` and declines to
    act on it, and a row must not be condemned here for what reconcile refuses
    to condemn.
    """
    from fno.agents.mux_spawn import _mux_pane_alive

    try:
        alive = _mux_pane_alive(mux)
    except Exception:  # noqa: BLE001 -- a broken probe must never condemn a row
        return None
    return "pane-gone" if alive is False else None


#: The ONE stored status that is a probe RESULT rather than a guess. Reconcile
#: writes it only after confirming the child was gone, and any later successful
#: interaction re-stamps the row ``live``, so it cannot strand a worker that
#: came back.
#:
#: Every other terminal-sounding value is excluded, and ``orphaned`` is the
#: sharp one: reconcile KEEPS the pid on an orphaned row precisely because that
#: process is still live but unowned, so condemning it from the stored word
#: would reap a running worker.
_STORED_EXITED = "exited"


def exit_falsifier(entry: Any) -> Optional[str]:
    """``"exit-recorded"`` when reconcile already proved this row's child gone.

    Needed because reconcile DESTROYS its own proof: the terminal ``Exited``
    transition nulls ``pid`` and ``pid_start_time`` (rightly -- a stale pid is
    itself a misleading liveness signal), which leaves a no-pid row that
    :func:`pid_falsifier` cannot condemn. A worker whose transcript is still
    warm would then classify ``reachable`` and ``resume`` would pick the dead
    attach path, which is the original false-live through a different door.

    This is not the stored enum becoming the answer. It is one affirmative
    probe result among the falsifiers, subject to the same rules: it can only
    lower a verdict, and only this single value qualifies.
    """
    stored = getattr(entry, "status", None)
    return "exit-recorded" if stored == _STORED_EXITED else None


def registry_falsifier(entry: Any) -> Optional[str]:
    """The falsifier a registry ROW carries, or None when it carries none.

    A mux-pane row is falsified by its PANE, never by its recorded pid. That pid
    is not the authority on the pane: ``reconcile`` re-derives the pane's current
    child on every pass because a mux restart can hand ``(session, pane_id)`` to
    a new child while the recorded pid dies, and it treats a live pane with no
    usable pid as INCONCLUSIVE, never dead. Falsifying such a row off the stale
    pid would condemn a healthy pane worker -- `list` renders it orphaned and
    `resume` refuses to attach it -- which is the reaping hazard this module
    exists to prevent, rebuilt one field over.

    Suppressing the stale pid is only half the rule, and stopping there trades
    one wrong answer for another: an exited pane IS provable, so dropping every
    falsifier leaves a dead pane reading ``unknown`` for as long as its
    transcript stays under the staleness window. Swap the wrong authority for
    the right one rather than removing it.

    A non-pane row has two falsifiers, not one, because reconcile destroys the
    first when it fires: proving the child gone nulls the pid, so the recorded
    exit is the only surviving evidence of a death this registry already
    observed. The exit tombstone is deliberately NOT consulted for a pane row --
    the pane is that row's authority, and a live pane must not be condemned by a
    stored word.

    Lives here rather than at either caller because BOTH ``fno agents list`` and
    ``fno agents truth`` read a registry row, and a second copy of this rule is
    how one of them ends up with a decorative guard.
    """
    mux = getattr(entry, "mux", None)
    if mux:
        return pane_falsifier(mux)
    return pid_falsifier(
        getattr(entry, "pid", None), getattr(entry, "pid_start_time", None)
    ) or exit_falsifier(entry)


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
    truth = resolve_session_truth(handle, stalled_after_s=stalled_after_s, **resolve_kwargs)
    return classify_reachability(
        truth_state=truth.get("state"),
        age_s=truth.get("last_activity_age_s"),
        falsifier=pid_falsifier(pid, pid_start_time),
    )
