"""One reachability derivation with a declared basis, behind every agents surface.

The full contract (one question per word, the four load-bearing rules, why
silence is never unreachable, why progress is a second axis, and why
transcript age is necessary but never sufficient) lives in
docs/architecture/reachability-contract.md. The rules this file's code must
not break: positive evidence comes only from transcript activity; falsifiers
are monotone toward unreachable; unknown is terminal and never coerced; the
basis and age travel with the verdict.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Optional, TypeGuard

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
#: Positive process evidence: the pid answered and is alive. Carries the owner
#: question's verdict on its own - a parked worker between turns holds its tree.
PROCESS = "process"
#: An active-looking state whose only age is a file mtime. Never positive
#: evidence (x-dead): parse the records, take the max, never stat.
MTIME_ONLY = "mtime-only"

#: The older wire vocabulary `--status` filtered on, before x-c672 replaced
#: the `live` token with served activity. Kept only as the historical note for
#: readers chasing an old `--status live` invocation; nothing renders these
#: words anymore. `rendered_activity` below is the one status word now, and it
#: answers what the session is DOING, not whether a store claims it live.
WIRE_STATUS = {
    REACHABLE: "live",
    UNREACHABLE: "orphaned",
    UNKNOWN: "unknown",
}

#: The attention window the activity word keys on: a transcript that moved
#: inside it is `writing`. Mirrors the daemon's `STALE_ATTENTION_S` (the
#: crates do not link, so the shared fixture in schemas/ is what pins them).
ACTIVITY_ATTENTION_S = 600


def rendered_activity(
    *,
    truth_state: Optional[str],
    age_s: Optional[float],
    reachability: str,
    provider_refusal: Optional[str] = None,
) -> str:
    """The STATUS word both ``fno agents list`` lanes render (x-c672, AC7).

    Served activity, never a ``live`` token: ``writing`` (the transcript
    moved inside :data:`ACTIVITY_ATTENTION_S`), ``quiet`` (older), ``parked``
    (the tail closed a promise), with the measured age riding the row in its
    own field. A positively falsified row reads ``orphaned``, and a probe
    that answered nothing reads ``unknown``. A row whose last assistant turn
    is a provider refusal reads ``refused``: the error record is the newest
    transcript entry, so without this arm a corpse reads ``writing``. Nothing
    DECIDES on this word: retirement reads the reverse join and the lanes
    read their own probes, so the column is free to answer the operator's
    actual question.
    """
    if reachability == UNREACHABLE:
        return "orphaned"
    if provider_refusal:
        return "refused"
    if truth_state == "done":
        return "parked"
    if age_s is None:
        return "unknown"
    return "writing" if age_s < ACTIVITY_ATTENTION_S else "quiet"

#: Truth states that are positive evidence of activity. ``done`` is deliberately
#: absent: a worker that emitted <promise> finished its MISSION, which says
#: nothing about whether it is still up, and conflating the two is how a
#: finished-but-live worker got reported unreachable.
_ACTIVE_STATES = frozenset({"working", "watching", "your-move"})

#: Transcript activity is positive evidence of liveness only while it is FRESH.
#: Past this, an active-looking tail records that a session WAS active, not that
#: it is now. Measured twice: a fleet of dead workers idle 20 to 58 minutes, all
#: reporting live (the STALE_ATTENTION_S note in session_truth), and node x-52d2
#: wedged behind a transcript quiet for 83 minutes that still read ``working``.
#: Deliberately not STALE_ATTENTION_S: that constant refuses to carry a
#: decision, and this one is a decision.
TRANSCRIPT_EVIDENCE_S = 20 * 60

#: Basis when an active tail resolved but is too old to certify liveness now.
#: UNKNOWN, never UNREACHABLE: an old transcript is absence of evidence.
STALE_TRANSCRIPT = "stale-transcript"


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
    fresh_s: float = TRANSCRIPT_EVIDENCE_S,
    pid_alive: Optional[bool] = None,
    last_activity_basis: Optional[str] = None,
) -> Reachability:
    """Pure classifier. ``falsifier`` is a basis string, or None for "did not fire".

    A probe that could not read its evidence must arrive here as ``None``, the
    same as a probe that read it and found the agent healthy: an unreadable pid
    must never falsify. ``pid_alive is True`` is positive process evidence
    (basis ``process``) - a parked worker between turns holds its tree while
    its transcript is quiet; the NEGATIVE never rides this parameter, a dead
    pid arrives as a ``falsifier`` (``pid_falsifier``). Monotone order:
    falsifier, then positive evidence, then absence.
    ``last_activity_basis="mtime"`` is never positive (x-dead: a file stamp
    moved 2h33m past its newest record); only a parsed record certifies
    liveness, so an active-looking state with an mtime age lands UNKNOWN.
    """
    if falsifier is not None:
        return Reachability(UNREACHABLE, falsifier, age_s)
    if pid_alive is True:
        return Reachability(REACHABLE, PROCESS, age_s)
    if truth_state in _ACTIVE_STATES:
        if last_activity_basis == "mtime":
            # Never positive: an mtime is a file stamp, not activity.
            return Reachability(UNKNOWN, MTIME_ONLY, age_s)
        # An unknowable age stays REACHABLE. Only POSITIVE evidence of staleness
        # demotes; a missing age is not evidence, and the monotone rule in this
        # module's header forbids lowering on absence.
        if age_s is not None and age_s > fresh_s:
            return Reachability(UNKNOWN, STALE_TRANSCRIPT, age_s)
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
#: The row's last assistant turn is a provider refusal the error taxonomy
#: classifies (x-e594). Distinct from ``model-refused``: that one is read off
#: observed-model evidence, this one off the transcript's own refusal text.
PROVIDER_REFUSED = "provider-refused"


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
    provider_refusal: Optional[str] = None,
) -> Progress:
    """Pure classifier for the progress axis. Never raises.

    Precedence, in order: a falsified/unreachable row first (a gone process
    has no progress state, and inventing one for it is the collapse again);
    then the refusal predicate (Locked Decision 3 -- structural, never reads
    the transcript's prose, so a reworded refusal message cannot break it);
    then the provider refusal the truth reading classified; then the
    truth-state arms plus the measured transcript age. A refusal
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
    if provider_refusal:
        return Progress(REFUSED, PROVIDER_REFUSED)
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


def mux_ref_names_a_pane(mux: Any) -> TypeGuard[dict]:
    """True when ``mux`` could name a real pane: a dict with a non-empty
    session and an integer pane_id of at least 1.

    A :data:`~typing.TypeGuard`, not a plain ``bool``: callers gate pane
    access on it (``entry.mux["session"]`` under ``if mux_ref_names_a_pane(
    entry.mux):``), and the checker must narrow the ref to a dict there the
    way the old truthiness test accidentally did.

    Pane ids are allocated from a floor of 1 -- ``pane_id_floor`` in
    ``crates/fno/src/server.rs`` returns ``persisted.max(registry_floor).max(1)``,
    and the pane-id claiming comment in ``mux_spawn.py`` states the same fact
    ("per server starting at 1") -- so a ``pane_id`` of 0 is not a dead pane,
    it is an unset field that leaked into the ref. Structural on purpose:
    ``registry_falsifier`` runs once per registry row on every ``fno agents
    list``, so a resolution probe here would hang one mux round-trip per row
    off a rendering path.

    ``bool`` is excluded explicitly because ``isinstance(True, int)`` is True
    in Python, and a ``pane_id`` of ``True`` is a leaked flag, not a pane.
    """
    if not isinstance(mux, dict):
        return False
    session = mux.get("session")
    if not isinstance(session, str) or not session:
        return False
    pane_id = mux.get("pane_id")
    if isinstance(pane_id, bool) or not isinstance(pane_id, int):
        return False
    return pane_id >= 1


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

    A row whose ref names no pane is not a pane row: it is judged by its pid
    and its exit tombstone exactly as a null-ref row is, because a ref that
    fails :func:`mux_ref_names_a_pane` is a wrong value, and a wrong value is
    worse than no value.

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
    if mux_ref_names_a_pane(mux):
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
        last_activity_basis=truth.get("last_activity_basis"),
    )


# ---------------------------------------------------------------------------
# (x-b029) The pane-identity cross-check
#
# One verb, two mismatch directions, counts always printed. Direction 1 finds
# a registry row whose mux ref points at a pane the listing cannot resolve to
# that row's identity (the stale pane a resume re-homed). Direction 2 finds
# the pane carrying fno's spawn signature in its argv with no registry row at
# all - the population this node is named for. Direction 2 reads ARGV only to
# RAISE A MISMATCH for an operator to judge; it never mints an identity from
# argv, which is the trap AGENTS.md records (argv can outlive the process it
# describes).

#: The argv flags fno's own spawn posture renders. Source of truth is the
#: harness builders (codex sandbox_flag's yolo arm, claude's permission_mode
#: else-arm); the literals are restated here because those builders are inline
#: in larger argv assemblies and a cross-check must not spawn argv to learn it.
FNO_SPAWN_SIGNATURE_FLAGS: tuple[str, ...] = (
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-skip-permissions",
)


def _process_tree() -> dict[int, str]:
    """One `ps -axo` snapshot: pid -> that process's argv joined with every
    DESCENDANT's argv, or an empty dict when ps cannot answer. Never raises.

    Two reasons for the shape. The tree read, not just the child: a pane's
    child_pid is usually the pane SHELL and the harness runs under it, so a
    child-only probe reads `/bin/zsh` for a pane holding a live fno worker
    and direction 2 never fires. And transitive, not one level: a launcher
    script between the shell and the harness must not hide the signature
    either. One snapshot serves every pane in a listing, so the cost is one
    process-table read per verb run, not per pane.
    """
    import subprocess

    try:
        out = subprocess.run(
            ["ps", "-axo", "ppid=,pid=,args="],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return {}
    if out.returncode != 0:
        return {}
    args_by_pid: dict[int, str] = {}
    children_of: dict[int, list[int]] = {}
    for line in out.stdout.splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) < 3:
            continue
        ppid_s, pid_s, args = parts
        try:
            ppid, child = int(ppid_s), int(pid_s)
        except ValueError:
            continue
        args_by_pid[child] = args
        children_of.setdefault(ppid, []).append(child)
    trees: dict[int, str] = {}
    for pid, own in args_by_pid.items():
        lines = [own]
        stack = list(children_of.get(pid, ()))
        seen: set[int] = set()
        while stack:
            descendant = stack.pop()
            if descendant in seen:
                continue
            seen.add(descendant)
            if descendant in args_by_pid:
                lines.append(args_by_pid[descendant])
            stack.extend(children_of.get(descendant, ()))
        trees[pid] = "\n".join(lines).strip()
    return trees


def pane_identity_crosscheck(
    panes: list[dict],
    rows: list[Any],
    session: str,
    argv_of=None,
) -> dict:
    """Compare one mux session's pane listing against the registry, both ways.

    ``panes`` is the parsed ``pane ls --json`` rows (each carrying ``fno_id``
    and, since x-b029, ``fno_id_state``); ``rows`` is the loaded registry.
    Pure aside from the injectable ``argv_of`` probe, so both directions and
    the counts-compared contract are unit-testable on constructed rows; the
    live-mux reading is the operator's run of the verb.

    A mismatch is a READING, never a repair: nothing here mutates the
    registry or the mux.
    """
    if argv_of is None:
        # One process-table read for the whole listing, not one per pane.
        tree = _process_tree()
        argv_of = tree.get

    pane_by_id = {p.get("pane_id"): p for p in panes}
    rows_with_mux = [r for r in rows if getattr(r, "mux", None)]
    rows_this_session = [
        r for r in rows_with_mux if (r.mux or {}).get("session") == session
    ]
    rows_other_session = len(rows_with_mux) - len(rows_this_session)

    # Direction 1: every row with a mux ref in THIS session resolves to a pane
    # whose fno_id matches the row's identity.
    row_mismatches: list[dict] = []
    for r in rows_this_session:
        mux = r.mux or {}
        pane_id = mux.get("pane_id")
        pane = pane_by_id.get(pane_id)
        expected = (getattr(r, "harness_session_id", None) or "").strip()
        if pane is None:
            row_mismatches.append(
                {
                    "row": r.name,
                    "pane": pane_id,
                    "reason": "pane missing from session listing",
                }
            )
            continue
        if not expected:
            row_mismatches.append(
                {
                    "row": r.name,
                    "pane": pane_id,
                    "reason": "row carries a mux ref but no session id",
                }
            )
            continue
        actual = pane.get("fno_id")
        state = pane.get("fno_id_state") or (
            "resolved" if actual else "unresolved:spawned-name"
        )
        if state != "resolved" or actual != expected:
            row_mismatches.append(
                {
                    "row": r.name,
                    "pane": pane_id,
                    "reason": f"pane id is {actual or state!r}, row expects {expected}",
                }
            )

    # Direction 2: every pane whose process tree carries fno's spawn
    # signature is referenced by a row of THIS session. Pane ids are
    # session-local, so a row of ANOTHER session naming the same numeric id
    # must not spare the pane. A resolved pane needs no signature check. The
    # probe reads argv ONLY to raise a mismatch; it never mints an identity
    # from argv.
    pane_mismatches: list[dict] = []
    referenced = {(r.mux or {}).get("pane_id") for r in rows_this_session}
    for p in panes:
        pane_id = p.get("pane_id")
        # A listing from an older daemon carries no fno_id_state; derive it
        # from the raw value so the check stays honest against both.
        state = p.get("fno_id_state") or (
            "resolved" if p.get("fno_id") else "unresolved:spawned-name"
        )
        if state == "resolved":
            continue
        if p.get("pristine_idle_shell"):
            # Positive shell-integrated evidence of an empty pane: a shell is
            # not a worker, and probing it would report every idle pane.
            continue
        pid = p.get("child_pid")
        if pid is None:
            continue
        argv = argv_of(pid)
        if not argv:
            continue
        if pane_id in referenced:
            continue
        if any(flag in argv for flag in FNO_SPAWN_SIGNATURE_FLAGS):
            pane_mismatches.append(
                {
                    "pane": pane_id,
                    "pid": pid,
                    "reason": "fno spawn signature in argv, no registry row",
                }
            )

    return {
        "session": session,
        "panes_compared": len(panes),
        "rows_with_mux_compared": len(rows_this_session),
        "rows_other_session": rows_other_session,
        "row_mismatches": row_mismatches,
        "pane_mismatches": pane_mismatches,
    }


def render_pane_identity_crosscheck(result: dict) -> str:
    """The human reading. The counts line prints on EVERY run, clean or not:
    a zero-mismatch result must be distinguishable from a check that never
    enumerated anything (AC5-EDGE)."""
    lines = [
        f"panes compared: {result['panes_compared']}; "
        f"rows with mux ref compared: {result['rows_with_mux_compared']}"
        + (
            f"; rows in other sessions skipped: {result['rows_other_session']}"
            if result["rows_other_session"]
            else ""
        )
    ]
    row_mismatches = result["row_mismatches"]
    pane_mismatches = result["pane_mismatches"]
    lines.append(f"row -> pane mismatches: {len(row_mismatches)}")
    for m in row_mismatches:
        lines.append(f"  row {m['row']} pane {m['pane']}: {m['reason']}")
    lines.append(f"pane -> row mismatches: {len(pane_mismatches)}")
    for m in pane_mismatches:
        lines.append(f"  pane {m['pane']} pid {m['pid']}: {m['reason']}")
    if not row_mismatches and not pane_mismatches:
        lines.append("clean: no mismatch in either direction")
    return "\n".join(lines)
