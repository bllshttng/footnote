"""SessionStart entry point: bind the current session to its registry row.

Invoked by ``hooks/register-session-start.sh`` as
``python3 -m fno.agents.register_session --harness claude ...``. Two modes,
selected by ``--agent-self``:

- without it, REGISTER an operator-started session (it has no row yet);
- with it, RESTAMP a footnote-spawned worker's existing row, named by
  ``FNO_AGENT_SELF``, onto the session id its harness is actually using. The
  id footnote passed at spawn is not durable, and registration keys its upsert
  on that same id, so a re-minted worker routed through registration would
  gain a second row rather than have its first corrected.

Fail-soft by contract (US7 AC7-ERR): any failure emits a
``session_register_failed`` / ``session_restamp_failed`` warning event and
still exits 0, so the hook never blocks session start even when the registry
is locked or unwritable. On success it emits ``session_registered`` /
``session_id_restamped`` and prints a one-line stderr note (hook stdout is
reserved for the session preamble).
"""
from __future__ import annotations

import argparse
import os
import sys
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional, Sequence

if TYPE_CHECKING:
    from fno.agents.reachability import Reachability

from fno.agents import events
from fno.agents.registry import (
    heal_mux_ref,
    register_existing_session,
    restamp_harness_session_id,
)
from fno.agents.spawn_defaults import resolve_lane_vendor

#: How long a spawned worker waits for its own row to appear before giving up.
#: Covers the spawner's post-`mux pane run` tail with room to spare: child-pid
#: lookup, the <=1s readiness probe, the seed submission, the registry lock.
#:
#: The seed can cost a second round on EITHER path, and the budget must cover
#: both. The pane-send path (agy) pays one `_SEED_RETRY_DELAY_S` sleep plus a
#: second `_submit_spawn_seed` and its mux RPCs. The argv path no longer reaches
#: that retry, but it pays the same sleep plus one read when a frame comes back
#: unreadable, which is the re-probe that keeps a healthy pane from being
#: condemned on one timed-out read. Same order of cost, so do not read the argv
#: path as free. Both fire exactly when panes paint slowest, so the delays
#: arrive together rather than independently. A miss costs
#: addressability, never the session: the hook exits 0 regardless. It leaves the
#: row on the spawn-time session id, which the watchdog ghost lane then reports
#: as `no transcript for <id>`, so widen this before trimming it.
_RESTAMP_ROW_WAIT_S = 10.0
_RESTAMP_ROW_POLL_S = 0.25


def _row_exists(name: str, harness: str) -> bool:
    """True when this worker's row is already in the registry.

    Fails to ``True`` (do not wait) on any read error: a registry we cannot read
    is not evidence that the row is missing, and sleeping on that guess would
    delay session start for a reason we cannot even state.
    """
    from fno.agents.registry import load_registry

    try:
        return any(
            (e.name == name or name in (getattr(e, "aliases", None) or []))
            and e.harness == harness
            for e in load_registry()
        )
    except Exception:  # noqa: BLE001 -- unreadable registry is not "row absent"
        return True


def _reading_for_entry(entry: Any) -> Optional["Reachability"]:
    """One row's family-1 reading, from the entry the caller already holds.

    The ONE reachability derivation every agents surface shares
    (``resolve_session_truth`` for the transcript evidence,
    ``registry_falsifier`` for the row's own falsifier - a mux row's pane,
    otherwise its pid/exit record), re-deriving neither. Returns ``None``
    when the evidence is unavailable, which classifies as deferred - never
    as death.
    """
    from fno.agents.reachability import classify_reachability, registry_falsifier
    from fno.agents.session_truth import resolve_session_truth

    try:
        truth = resolve_session_truth(entry.name)
        return classify_reachability(
            truth_state=truth.get("state"),
            age_s=truth.get("last_activity_age_s"),
            falsifier=registry_falsifier(entry),
        )
    except Exception:
        return None


def _predecessor_reading(
    name: str, harness: str
) -> tuple[Optional[str], Optional["Reachability"]]:
    """Sample the row's recorded id and its family-1 truth reading.

    Returns ``(recorded_session_id, Reachability | None)``; ``None`` reading
    means the evidence is unavailable.
    """
    from fno.agents.registry import load_registry

    try:
        entry = next(
            (
                candidate
                for candidate in load_registry()
                if candidate.harness == harness
                and (candidate.name == name or name in candidate.aliases)
            ),
            None,
        )
    except Exception:
        return None, None
    if entry is None or not entry.harness_session_id:
        return None, None

    return entry.harness_session_id, _reading_for_entry(entry)


def _predecessor_observation(
    name: str, harness: str
) -> tuple[Optional[str], Optional[bool]]:
    """Return the sampled predecessor ID and transcript reachability."""
    recorded, reading = _predecessor_reading(name, harness)
    return recorded, _verdict_bool(reading)


def _mux_pair_from_env(env: "Optional[dict[str, str]]" = None) -> Optional[tuple[str, int]]:
    """The hosting pair a pane worker reads from its own environment.

    ``FNO_SESSION`` (the mux session) and ``FNO_PANE`` (the pane's own id)
    are in every pane child's env, so a worker can name the pane it runs in
    with no new IPC. A bg/headless worker has neither var and must never
    gain a mux ref, so anything less than BOTH vars present is None, as is a
    non-numeric or negative ``FNO_PANE`` (a u64 as a string is never
    coerced).
    """
    env = os.environ if env is None else env
    session = (env.get("FNO_SESSION") or "").strip()
    pane_raw = (env.get("FNO_PANE") or "").strip()
    if not session or not pane_raw:
        return None
    try:
        pane = int(pane_raw)
    except ValueError:
        return None
    if pane < 0:
        return None
    return session, pane


def _heal_own_mux_ref(agent_self: str, harness: str, session_id: str) -> None:
    """x-0345 W2 (identity OUT): heal this worker's row to the pane it runs in.

    Runs inside the fail-open restamp path, AFTER the id observation so the
    heal targets the row that names THIS session - the classification can
    branch (a reachable predecessor keeps its row, this session mints its
    own), and keying on the name alone would heal the predecessor while the
    successor stays paneless. `heal_mux_ref` prefers the row whose
    harness_session_id is `session_id` and falls back to the name. Every
    door that can put a spawned worker on a pane routes through here -
    resume, a pasted `--print-command` line, a hand-written `mux pane run` -
    because the hook runs INSIDE the pane rather than beside it. The success
    event is emitted only on a verified write (heal_mux_ref confirms the
    persisted row); a swallowed failure leaves
    `session_pane_rebound_failed` on the event log.
    """
    pair = _mux_pair_from_env()
    if pair is None:
        return
    try:
        moved = heal_mux_ref(
            name=agent_self,
            harness=harness,
            mux_session=pair[0],
            pane_id=pair[1],
            session_id=session_id,
        )
    except Exception as exc:  # fail-open: never block session start (AC7-ERR)
        events.emit(
            "session_pane_rebound_failed",
            provider=harness,
            name=agent_self,
            session=f"{pair[0]}:{pair[1]}",
            error=str(exc),
        )
        print(f"register_session: warning: {exc}", file=sys.stderr)
        return
    if moved is not None:
        old_mux, new_mux = moved
        events.emit(
            "session_pane_rebound",
            provider=harness,
            name=agent_self,
            old=old_mux,
            new=new_mux,
        )


def _restamp(agent_self: str, harness: str, session_id: str, source: str = "") -> int:
    """Bind a SPAWNED worker's SessionStart id observation to its own row.

    Split from registration because the two answer different questions. A
    spawned worker already HAS a row; the only open question is which session
    id or ids it records.

    For a claude row the observation is ADDITIVE (one row, at most one
    optional related id, no lineage): an empty primary fills, a second
    different id fills the related slot without touching the primary, and a
    third distinct id refuses the write while naming the two recorded ids.
    The hook stays fail-soft - the refusal is a visible event, never a
    blocked session - while the registry mutation itself fails closed.

    Other harnesses keep the lineage-aware restamp: this contract is
    claude-only, and their rows carry no related slot.

    RETRIES on a missing row, briefly. The spawner creates the row AFTER
    ``mux pane run`` returns (it needs the pane id, and a half-created row is
    worse than a late one), so a worker whose harness boots fast enough can run
    this hook before its own row exists. `restamp_harness_session_id` cannot
    distinguish "no such row" from "not yet" and returns None either way, so
    without a wait the handoff silently loses the race. That is unrecoverable on
    any route where this restamp is the ONLY path to an id - a happy-hosted
    claude pane cannot be given one at spawn, because happy discards it - and the
    worker would stay id-less and `spawning` for its whole life while being
    perfectly healthy. The wait is bounded and still fail-soft: exhausting it
    returns 0 exactly as a genuine no-row miss always did.
    """
    # Only the pane substrate writes its row after the child starts, and it says
    # so by name. Every other spawn either has its row already or never gets one -
    # a headless one-shot sets FNO_AGENT_SELF and deliberately has no row, so
    # waiting on that signal alone would delay every one-shot's first prompt by
    # the full deadline for a row that is never coming.
    #
    # Comparing to THIS worker's name, not just testing presence: a pane worker
    # passes its whole environment to any one-shot it launches, so the marker is
    # inherited by children that are not the pane. It names the pane, the child
    # overwrites FNO_AGENT_SELF with its own name, and the mismatch cancels the
    # wait without any spawn path having to remember to clear it.
    deadline = time.monotonic() + (
        _RESTAMP_ROW_WAIT_S
        if os.environ.get("FNO_AGENT_ROW_PENDING") == agent_self
        else 0.0
    )
    try:
        while True:
            # Observe the row BEFORE restamping, not after. None from the restamp
            # is ambiguous - no such row YET, or a row that already matches - and
            # only the first is worth waiting on. Reading existence AFTERWARDS
            # answers the wrong instant: the spawner can append the row in between,
            # and we would then break on "a row exists" having never restamped
            # THAT row, stranding the worker id-less for life. Checked first, a
            # True can only mean the row predates this restamp, so a None beside
            # it really is "already current".
            existed = _row_exists(agent_self, harness)
            if harness == "claude":
                from fno.agents.registry import record_session_observation

                expected, reading = _reading_for_transition(
                    agent_self, harness, session_id
                )
                entry, outcome = record_session_observation(
                    name=agent_self,
                    harness=harness,
                    session_id=session_id,
                    predecessor_reachable=_verdict_bool(reading),
                    expected_predecessor_session_id=expected,
                )
                if outcome != "no-row" or existed:
                    _report_observation(
                        agent_self, harness, session_id, source, entry, outcome
                    )
                    _emit_transition_events(
                        agent_self,
                        harness,
                        session_id,
                        source,
                        entry,
                        outcome,
                        reading,
                    )
                    # x-0345 W2 (identity OUT): heal the row's pane ref from
                    # the hosting pair this session reads from its own env -
                    # AFTER the id observation, so a branched session heals
                    # its OWN row, not the predecessor's.
                    _heal_own_mux_ref(agent_self, harness, session_id)
                    return 0
            else:
                expected_predecessor_session_id, predecessor_reachable = (
                    _predecessor_observation(agent_self, harness)
                )
                transitions: list = []
                entry = restamp_harness_session_id(
                    name=agent_self,
                    harness=harness,
                    session_id=session_id,
                    predecessor_reachable=predecessor_reachable,
                    expected_predecessor_session_id=expected_predecessor_session_id,
                    transitions=transitions,
                )
                for applied in transitions:
                    _emit_and_ask_on_branch(
                        name=applied.get("name") or agent_self,
                        harness=harness,
                        classification=applied["classification"],
                        predecessor=applied["predecessor"],
                        successor=applied["successor"],
                        source=source,
                    )
                if entry is not None or existed:
                    break
            if time.monotonic() >= deadline:
                break
            time.sleep(_RESTAMP_ROW_POLL_S)
    except Exception as exc:  # fail-open: never block session start (AC7-ERR)
        events.emit(
            "session_restamp_failed",
            provider=harness,
            name=agent_self,
            session_id=session_id,
            error=str(exc),
        )
        print(f"register_session: warning: {exc}", file=sys.stderr)
        return 0

    # None means nothing needed doing (id already current, or no such row) --
    # the overwhelmingly common case, so it stays silent on both channels.
    if entry is not None:
        events.emit(
            "session_id_restamped",
            provider=harness,
            name=entry.name,
            session_id=session_id,
        )
        print(
            f"register_session: restamped {entry.name} -> {session_id}",
            file=sys.stderr,
        )
    # x-0345 W2: the non-claude lineage path heals its pane ref here, after
    # its restamp, for the same branched-row reason the claude arm heals
    # inside its own arm.
    _heal_own_mux_ref(agent_self, harness, session_id)
    return 0


def _verdict_bool(reading: Optional["Reachability"]) -> Optional[bool]:
    """The classifier's tri-state from one reachability reading.

    REACHABLE is True, UNREACHABLE is False, and UNKNOWN (or no reading at
    all) is None: unknown is terminal and must never be coerced to either
    pole - a coerced unknown is how a healthy row gets declared dead.
    """
    from fno.agents.reachability import REACHABLE, UNREACHABLE

    if reading is None:
        return None
    if reading.verdict == REACHABLE:
        return True
    if reading.verdict == UNREACHABLE:
        return False
    return None


def _reading_for_transition(
    name: str, harness: str, session_id: str
) -> tuple[Optional[str], Optional["Reachability"]]:
    """Sample family-1 evidence ONLY when the payload could be a transition.

    A same-id SessionStart (every healthy restart, every compact) can never
    reclassify anything, so it pays no transcript read. A different id
    samples the row's recorded id plus its reachability reading, for the
    compare-and-swap guard and the classifier respectively.
    """
    from fno.agents.registry import load_registry

    try:
        entry = next(
            (
                candidate
                for candidate in load_registry()
                if candidate.harness == harness
                and (candidate.name == name or name in candidate.aliases)
            ),
            None,
        )
    except Exception:
        return None, None
    if entry is None:
        return None, None
    primary = entry.harness_session_id or ""
    if not primary or primary == session_id:
        return None, None
    # The row is in hand; the reading derives from it without a second load.
    return primary, _reading_for_entry(entry)


def _predecessor_of_outcome(entry: object, outcome: str, successor: str) -> Optional[str]:
    """The A a finished observation retired, parked, or forked from."""
    if entry is None:
        return None
    if outcome == "succession":
        predecessors = getattr(entry, "predecessor_session_ids", None) or []
        return predecessors[-1] if predecessors else None
    if outcome == "branch":
        return getattr(entry, "forked_from_session_id", None)
    return getattr(entry, "harness_session_id", None) or None


def _emit_transition_events(
    name: str,
    harness: str,
    session_id: str,
    source: str,
    entry: object,
    outcome: str,
    reading: Optional["Reachability"],
) -> None:
    """Emit the typed transition event for one finished observation.

    Succession and branch classify; a second id parked in the related slot
    under unavailable evidence defers, positively naming WHY. The
    idempotency replay (the payload's id already primary, its predecessor in
    the chain) emits the already-applied marker instead of a second
    classified event. Every helper here is best-effort by construction.
    """
    from fno.agents.events import (
        KIND_SESSION_TRANSITION_ALREADY_APPLIED,
        KIND_SESSION_TRANSITION_CLASSIFIED,
        KIND_SESSION_TRANSITION_DEFERRED,
        emit_session_transition,
    )

    try:
        if outcome == "no-op":
            predecessors = getattr(entry, "predecessor_session_ids", None) or []
            if (
                predecessors
                and (getattr(entry, "harness_session_id", None) or "") == session_id
            ):
                emit_session_transition(
                    KIND_SESSION_TRANSITION_ALREADY_APPLIED,
                    name=name,
                    harness=harness,
                    predecessor_session_id=predecessors[-1],
                    successor_session_id=session_id,
                    source=source or None,
                )
            return
        predecessor = _predecessor_of_outcome(entry, outcome, session_id)
        if not predecessor:
            return
        if outcome in ("succession", "branch"):
            emit_session_transition(
                KIND_SESSION_TRANSITION_CLASSIFIED,
                name=name,
                harness=harness,
                predecessor_session_id=predecessor,
                successor_session_id=session_id,
                classification=outcome,
                basis=getattr(reading, "basis", None),
                source=source or None,
            )
            if outcome == "branch":
                _record_branch_question(
                    name=name,
                    harness=harness,
                    predecessor=predecessor,
                    successor=session_id,
                    branch_name=getattr(entry, "name", name),
                )
        elif outcome == "related" and reading is not None:
            # Evidence was sampled and came back unavailable: the id is
            # parked additively and reconciliation retries later. No
            # operator question and no overwrite (AC3-ERR).
            emit_session_transition(
                KIND_SESSION_TRANSITION_DEFERRED,
                name=name,
                harness=harness,
                predecessor_session_id=predecessor,
                successor_session_id=session_id,
                basis=getattr(reading, "basis", None),
                source=source or None,
            )
    except Exception:  # noqa: BLE001 -- observability never blocks session start
        pass


_BRANCH_QUESTION_MARKER = "session-transition-branch"


def _branch_question_open(root, key: str) -> bool:
    from fno.agents.stale_escalate import already_asked

    return already_asked(root, key, marker=_BRANCH_QUESTION_MARKER) is not None


def _record_branch_question(
    *, name: str, harness: str, predecessor: str, successor: str, branch_name: str
) -> None:
    """Record the branch fork's ONE durable operator question.

    The only transition that asks anything: should the new live session
    inherit the node and its claim, or start clean? Exactly two options,
    recorded once per fork edge - a re-observed branch never re-asks.
    """
    import secrets

    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    # The hook cds to the repo root before invoking this module, so the
    # session's project journal is the cwd one; the machine-wide question
    # index gets the same event and is what the dedupe read folds.
    root = Path(os.getcwd())
    key = f"{name}:{predecessor}:{successor}"
    if _branch_question_open(root, key):
        return
    append_question_event(
        operator_question(
            question_id=f"q-{secrets.token_hex(4)}",
            question=(
                f"[{_BRANCH_QUESTION_MARKER}:{key}] "
                f"Branch {branch_name} ({harness}) forked live session "
                f"{predecessor} into {successor}. Should it inherit the node "
                f"and claim, or start clean?"
            ),
            session_id=successor,
            ask=(
                "answer inherit or clean; inherit proceeds only through the "
                "voluntary claim-handover path and never force-releases the "
                "live predecessor"
            ),
            options=["inherit node and claim", "start clean"],
            source="hook",
        ),
        root,
    )


def _emit_and_ask_on_branch(
    *, name: str, harness: str, classification: str, predecessor: str, successor: str, source: str
) -> None:
    """Emit one applied non-claude restamp transition, asking on a branch."""
    from fno.agents.events import (
        KIND_SESSION_TRANSITION_CLASSIFIED,
        emit_session_transition,
    )

    try:
        emit_session_transition(
            KIND_SESSION_TRANSITION_CLASSIFIED,
            name=name,
            harness=harness,
            predecessor_session_id=predecessor,
            successor_session_id=successor,
            classification=classification,
            source=source or None,
        )
        if classification == "branch":
            _record_branch_question(
                name=name,
                harness=harness,
                predecessor=predecessor,
                successor=successor,
                branch_name=name,
            )
    except Exception:  # noqa: BLE001 -- observability never blocks session start
        pass


def _report_observation(
    agent_self: str,
    harness: str,
    session_id: str,
    source: str,
    entry: object,
    outcome: str,
) -> None:
    """Surface one claude SessionStart observation's outcome, by name.

    The registry mutation already failed closed inside the recorder; this is
    the visible half. The cap refusal carries both recorded ids - the
    operator's next action is choosing between them, so they are the payload.
    A no-op stays silent on both channels (the common case: every subsequent
    SessionStart of a healthy worker).
    """
    name = getattr(entry, "name", agent_self)
    if outcome in ("no-op", "no-row"):
        return
    if outcome == "refused-cap":
        primary = getattr(entry, "harness_session_id", "") or "-"
        related = getattr(entry, "related_session_id", "") or "-"
        events.emit(
            "session_id_record_refused",
            provider=harness,
            name=name,
            session_id=session_id,
            recorded_ids=f"{primary},{related}",
            source=source or None,
        )
        print(
            f"register_session: warning: {name} already records two session "
            f"ids ({primary}, {related}); not recording a third "
            f"({session_id})",
            file=sys.stderr,
        )
        return
    events.emit(
        "session_id_recorded",
        provider=harness,
        name=name,
        session_id=session_id,
        outcome=outcome,
        source=source or None,
    )
    print(
        f"register_session: recorded {name} {outcome} id {session_id}",
        file=sys.stderr,
    )


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="register_session")
    # --harness is canonical; --provider is the axis-rename alias (x-bab1), kept
    # so the fail-soft SessionStart hook keeps working across the cutover.
    parser.add_argument("--harness", dest="harness",
                        help="Harness/CLI identity to register (claude | codex | gemini).")
    parser.add_argument("--provider", dest="harness", help=argparse.SUPPRESS)
    parser.add_argument("--session-id", required=True)
    parser.add_argument("--cwd", required=True)
    parser.add_argument(
        "--source",
        default="",
        help="The harness SessionStart flavor (claude: startup | resume | clear), "
        "threaded into the observation events for observability.",
    )
    parser.add_argument("--name", default=None)
    parser.add_argument("--log-path", default="")
    parser.add_argument(
        "--agent-self",
        default=None,
        help="This worker's own registry row name (FNO_AGENT_SELF). Set only for "
        "a footnote-SPAWNED worker; switches this call from register to restamp.",
    )
    args = parser.parse_args(argv)

    if not args.harness:
        parser.error("--harness is required")

    # An empty session id reaches here when the hook's CLI env var is unset
    # (non-claude harness, or claude not exporting it). Treat as a silent
    # no-op rather than a noisy failure event: there is nothing to register.
    if not args.session_id:
        return 0

    if args.agent_self:
        return _restamp(
            args.agent_self, args.harness, args.session_id, source=args.source
        )

    try:
        vendor = resolve_lane_vendor([args.harness], env=os.environ, harness=args.harness)
        entry = register_existing_session(
            session_id=args.session_id,
            cwd=args.cwd,
            harness=args.harness,
            provider=vendor,
            name=args.name or None,
            log_path=args.log_path,
            origin="operator",
            # x-98ab: the row describes THIS session, so its own exported
            # FNO_NODE (a node-driven pane carries one) is the right source.
            node=(os.environ.get("FNO_NODE") or "").strip() or None,
        )
    except Exception as exc:  # fail-open: never block session start (AC7-ERR)
        events.emit(
            "session_register_failed",
            provider=args.harness,
            session_id=args.session_id,
            error=str(exc),
        )
        print(f"register_session: warning: {exc}", file=sys.stderr)
        return 0

    events.emit(
        "session_registered",
        provider=entry.provider,
        harness=entry.harness,
        name=entry.name,
        session_id=args.session_id,
        cwd=entry.cwd,
    )
    print(f"register_session: registered {entry.name} ({entry.harness})", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
