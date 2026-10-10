"""What a durable mail receipt says about the recipient's reachability.

Extracted from ``fno.mail.cli`` (file-budget: that module is shrink-only) so
the demotion-receipt logic has a home named by the question it answers: the
stderr recovery warning every durable floor prints, its live-lane reason
vocabulary, and the transcript-age suffix a bare live-miss carries.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from typing import Optional

# The exit a NOT LANDED receipt leaves `mail send` with : a last-line
# reader must never record an unconfirmed send as delivered. Distinct from the
# usage (2), lock (11), durable-address (12) and unknown-agent (16) refusals:
# the send itself succeeded and is recoverable, the LANDING is what is missing.
NOT_LANDED_EXIT = 14


def _render(argv: list[str]) -> str:
    """One mail-receipt read through the native door (the style-check
    pattern: Python keeps transports, the assembly lives in Rust). The
    receipt vocabulary is single-source, so a missing binary refuses rather
    than falling back to a second renderer."""
    from fno.rust_binary import VerbUnavailable, resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise VerbUnavailable("fno-agents binary not found; run `fno doctor update`")
    proc = subprocess.run(
        [str(binary), "mail-receipt", *argv],
        capture_output=True,
        text=True,
        timeout=10,
    )
    if proc.returncode != 0:
        raise VerbUnavailable(
            (proc.stderr or "fno-agents mail-receipt failed").strip()[:200]
        )
    return proc.stdout.rstrip("\n")


def _flag(name: str, value: Optional[str]) -> list[str]:
    return [f"--{name}", value] if value is not None else []


def _live_miss_age_suffix(recipient: str) -> str:
    """The transcript-age suffix a live-miss or transcript- reason carries (AC8).

    A bare live-miss reads the same for a transient miss to a genuinely live
    peer (re-send works) and for a session that stood down hours ago (nothing
    will drain it); the age is the discriminator. An unreadable transcript
    prints unknown, never 0s. Three lanes emit that receipt (the name lane,
    the job lane, the registered-agent lane), so the suffix lives here once.
    """
    from fno.agents.session_truth import fmt_age, resolve_session_truth

    age_s = resolve_session_truth(recipient).get("last_activity_age_s")
    if age_s is None:
        return ", transcript age unknown"
    return f", transcript quiet {fmt_age(age_s)}"


# Live-lane failures where the recipient WAS live and reachable but the inject
# did not confirm (node). For these the durable preamble must NOT say
# "is not live" -- the recipient was live, so that wording read as a liveness
# lie and cost a wrong hypothesis on measured evidence. The receipt names the
# real cause instead. ``no-confirm-source`` joins that set: the keeper
# lane refuses it only AFTER the socket connect, so the recipient was reachable
# and the honest receipt is "unconfirmed", not "is not live".
_LIVE_LANE_FAILURE_REASONS = frozenset(
    {
        "not-confirmed",
        "attach-failed",
        "io-error",
        "mux-send-failed",
        "unsafe-text",
        "no-confirm-source",
        "left-in-composer",
    }
)


def _is_live_lane_failure(reason: Optional[str]) -> bool:
    if not reason:
        return False
    return any(
        token in _LIVE_LANE_FAILURE_REASONS or token.startswith("mux-send-failed-")
        for token in reason.split(";")
    )


def durable_window_clause(owner: Optional[str]) -> str:
    """The drain-window tail every ``queued (durable)`` receipt carries.

    ``queued (durable)`` says nothing about time, so "not yet" and "never"
    read identically. The tail quotes the owner-class horizon the stranded
    sweep enforces (one bound, one table); an unknown class prints nothing,
    since a guessed constant is this defect in friendlier dress.
    """
    from fno.inbox.store import owner_ttl_hours

    hours = owner_ttl_hours(owner or "")
    return _render(["window", "--ttl-hours", repr(hours)])


def durable_leg_story(
    reason: Optional[str], recipient: Optional[str] = None
) -> Optional[str]:
    """Positive stdout wording for a live-lane failure demotion : a
    live-inject miss plus a durable success is a normal outcome, and the raw
    token (``io-error``, ``attach-failed``, ...) rendered an error string
    inside a success receipt. None when the reason is not a live-lane
    failure; the token stays diagnostic (stderr advisory, bus record).

    ``recipient``, when passed, appends how long the live leg waited
    (a ``waited-<n>s`` token in the reason) and the transcript-age suffix, so
    the sender can tell a busy peer from a silent one. Words only: no raw
    token reaches this line."""
    suffix = _live_miss_age_suffix(recipient) if recipient is not None else None
    story = _render(
        ["story", *(_flag("reason", reason)), *(_flag("suffix", suffix)), *(_flag("age-of", recipient))]
    )
    return story or None


def json_receipt(
    msg_id: str,
    *,
    to: str,
    status: str,
    subject: Optional[str] = None,
) -> str:
    """The default send receipt: one JSON line, SendMessage-shaped.

    Four keys, always: ``msg_id`` (the reply handle), ``subject`` (the
    sender's ``--subject``, null when none), ``to``, and ``status`` carrying
    the delivery verdict in the receipt vocabulary every reader already
    greps (``delivered (hosted)``, ``queued (durable)``, ``typed``)."""
    return _render(
        [
            "json",
            "--msg-id", msg_id,
            "--to", to,
            "--status", status,
            *(_flag("subject", subject)),
        ]
    )


def demotion_receipt(
    msg_id: str,
    *,
    reason: Optional[str],
    owner: Optional[str],
    target: Optional[str] = None,
    project: Optional[str] = None,
    age_target: Optional[str] = None,
    subject: Optional[str] = None,
) -> str:
    """The stdout receipt for a durable demotion (refined by x-aaaa)."""
    from fno.inbox.store import owner_ttl_hours

    age_of = age_target if age_target is not None else target
    suffix = _live_miss_age_suffix(age_of) if age_of is not None else None
    return _render(
        [
            "demotion",
            "--msg-id", msg_id,
            *(_flag("reason", reason)),
            "--ttl-hours", repr(owner_ttl_hours(owner or "")),
            *(_flag("target", target)),
            *(_flag("project", project)),
            *(_flag("age-of", age_of)),
            *(_flag("suffix", suffix)),
            *(_flag("subject", subject)),
        ]
    )


def not_landed_receipt(
    msg_id: str,
    pane: Optional[int],
    *,
    target: str,
    harness: Optional[str] = None,
    session_id: Optional[str] = None,
) -> str:
    """The NOT LANDED verdict block for a durable-floor send the post-send
    verify could not confirm .

    ``pane`` comes from the LIVE pane list via :func:`_live_pane_for`, never
    the registry row: a row can carry no mux ref while the pane list still
    shows the recipient's pane (measured: mux=None, pane alive). A codex
    thread with no pane cannot be injected by fno at all - the session's own
    surface has to receive it - and the verify names that.
    """
    return _render(
        [
            "not-landed",
            "--msg-id", msg_id,
            *(_flag("pane", str(pane) if pane is not None else None)),
            "--target", target,
            *(_flag("harness", harness)),
            *(_flag("session-id", session_id)),
        ]
    )


def report_landing(
    msg_id: str,
    *,
    target: str,
    to: Optional[str],
    to_harness: Optional[str],
    to_session: Optional[str],
) -> bool:
    """The post-send verify both durable floors share: after the settle
    window, re-read the recipient - bus claim and cursor, then transcript -
    and print `landed (<how>)` or the NOT LANDED block with the
    substrate-correct recovery. True when landed; the caller owns the
    non-zero exit."""
    import time as _time

    from fno.mail.landed import post_send_landed, post_send_settle_seconds

    settle_s = post_send_settle_seconds()
    if settle_s > 0:
        _time.sleep(settle_s)
    landed, how = post_send_landed(
        msg_id, to=to, to_harness=to_harness, to_session=to_session
    )
    if landed:
        print(f"{msg_id} landed ({how})")
        return True
    print(not_landed_receipt(
        msg_id,
        _live_pane_for(target, to_session),
        target=target,
        harness=to_harness,
        session_id=to_session,
    ))
    return False


def _live_pane_for(target: str, session_id: Optional[str]) -> Optional[int]:
    """The recipient's pane id from the LIVE pane list, or None.

    Matches on harness session id (the stable join) or the pane's label,
    in one ``pane ls --json`` read. Any failure - mux down, unreadable
    JSON - answers None: a recovery hint that guesses a pane number is
    worse than a generic verify line.
    """
    from fno.agents.mux_spawn import DispatchAskError, _run_mux

    try:
        proc = _run_mux(["mux", "pane", "ls", "--json"], subprocess.run)
        if proc.returncode != 0:
            return None
        rows = json.loads(proc.stdout or "[]")
    except (DispatchAskError, OSError, TypeError, ValueError):
        return None
    if not isinstance(rows, list):
        return None
    for row in rows:
        if not isinstance(row, dict):
            continue
        # The session id is the stable join; the label matches only when no
        # session id exists, so a shell pane that happens to share the
        # recipient's name can never produce a wrong pane number.
        if session_id:
            hit = row.get("fno_id") == session_id or row.get("harness_session_id") == session_id
        else:
            hit = bool(target) and row.get("name") == target
        if hit:
            pane = row.get("pane_id")
            if isinstance(pane, int):
                return pane
    return None


def print_project_demotion(result, to_project: str, subject: Optional[str] = None) -> None:
    """Stdout receipt(s) for a --to-project send that wrote durable.

    A resolved live peer demoted to durable is addressed to that PEER (same
    dispatch_send as the by-name lane, same cause); a bus-only peer gets the
    designed-queue receipt; no peer queues to the project inbox itself.
    """
    if result.recipient is not None:
        from fno.agents.dispatch import BUS_ONLY_POLICY

        if result.reason == BUS_ONLY_POLICY:
            from fno.mail import hold as _hold

            _note = _hold.bounce_reason(result.recipient)
            print(json_receipt(
                result.msg_id,
                to=result.recipient,
                status=(
                    f"queued (durable) [project {to_project}] "
                    f"[{_note or 'DND (bus-only): recipient polls the bus at each turn boundary'}]"
                )
                + durable_window_clause(result.durable_owner),
                subject=subject,
            ))
            if _note:
                print(f"`fno agents mail withdraw {result.msg_id}` retracts it.")
        else:
            _warn_deferred(result.recipient, reason=result.reason)
            print(demotion_receipt(
                result.msg_id,
                reason=result.reason, owner=result.durable_owner,
                target=result.recipient, project=to_project,
                subject=subject,
            ))
        return
    from fno.inbox.store import DurableOwner

    _warn_deferred(to_project, project=True)
    print(json_receipt(
        result.msg_id,
        to=to_project,
        status=(
            "queued (durable) [param-forced: --to-project]"
            + durable_window_clause(DurableOwner.INBOX_DRAIN.value)
        ),
        subject=subject,
    ))


def _warn_deferred(target: str, *, project: bool = False, reason: Optional[str] = None) -> None:
    """Fail loud on a dead-letter miss: the envelope hit only the durable floor
    with no live inject path, so the sender learns delivery deferred instead of
    the message vanishing silently until the recipient's next SessionStart drain.

    The durable copy is RECOVERY, not delivery - it waits on a drain the
    recipient may never run. So this names the recovery ladder rather than
    leaving the sender to wait: a session that is merely idle can be brought
    back and re-sent to immediately, which beats waiting on a drain every time.

    It leads with `peek`, not `resume`, because the fallback fires on an
    UNCONFIRMED live inject, not a proven failure: a busy recipient can record
    the injected turn past the confirm budget and receive it anyway, so a blind
    re-send is the documented double-delivery edge rather than a fix.

    ``reason`` is the live lane's own cause (node). When it names a
    live-lane failure (see :data:`_LIVE_LANE_FAILURE_REASONS`) the recipient WAS
    live and reachable, so the preamble says so and names the cause rather than
    claiming "is not live" -- a receipt naming the wrong cause is worse than one
    naming none, because it sends the reader to diagnose a recipient that was
    never the problem. A None or unreachable reason repeats a transcript verdict.

    A lock timeout gets its own arm for the same reason. The per-agent flock is
    shared by every verb that touches the agent (send, ask, spawn, stop, rm), so
    a timeout says nothing about the recipient's liveness in EITHER direction.
    The not-live copy would send the reader to resurrect a session that is
    working fine; naming the holder a peer sender would tell the reader a
    just-stopped session is fine. The arm names neither and points at `peek`.

    Warning only - the durable enqueue succeeded, so exit stays 0."""
    from fno.agents.dispatch import LOCK_TIMEOUT_REASON
    from fno.mail.deferred_liveness import deferred_liveness_head

    if project:
        arm = "project"
    elif reason == LOCK_TIMEOUT_REASON:
        arm = "lock"
    elif _is_live_lane_failure(reason):
        arm = "live"
    else:
        arm = "plain"
    head = deferred_liveness_head(target) if arm == "plain" else None
    print(
        _render(
            [
                "warn-deferred",
                "--target", target,
                "--arm", arm,
                *(_flag("reason", reason)),
                *(_flag("head", head)),
            ]
        ),
        file=sys.stderr,
    )


# Send-time human escalation for a question, per (sender, recipient). A burst
# re-nudges every window rather than once forever (marker refreshed only on an
# actual escalation, so the window runs from the last nudge, not the first send).
_ESCALATION_DEBOUNCE_S = 300


def _recipient_is_attended(recipient: str) -> bool:
    """True iff ``recipient``'s registry row was stamped ``origin=operator`` at
    a hand-start (SessionStart register hook / ``fno agents register``).

    Attendance is declared at registration, never inferred at send time, so a
    row missing the field (a spawn/host worker, or a pre-change row) reads as
    not-attended -- fail toward silence. Never raises: an unreadable registry or
    an unresolved recipient escalates nothing, so the send still succeeds.
    """
    try:
        return _render(["attended", "--recipient", recipient]) == "true"
    except Exception:  # noqa: BLE001 - a door failure escalates nothing
        return False


def _escalate_to_human(
    sender: str,
    recipient: str,
    summary: str,
    reason: str,
    msg_id: str | None = None,
) -> str:
    """Notify the human at send time that mail needs them, and surface it in the
    needs-me mux overlay.

    ``reason`` is ``"question"`` (a --kind question send; Locked Decision 7: a
    question NEVER autonomous-responds - only the human answers it) or
    ``"attended-miss"`` (a send to an operator-attended session that fell to the
    durable floor) or ``"reachable-miss"`` (the same miss to a worker the
    resolver reports reachable). Every reason flows through this ONE helper so
    the overlay event is emitted from a single place; a second emit site would
    leave one reason un-surfaced (the silent-eat this exists to close). A reason
    added here must also be added to :data:`fno.events.MAIL_ESCALATION_REASONS`
    and the schema enum, or the overlay emit raises and is swallowed by the
    best-effort guard below - the nudge then reaches the notifier only.

    Debounced per (sender, recipient) so a chatty peer cannot spam the queue, and
    the debounce gates BOTH the notifier and the event (one event per
    non-debounced escalation, zero on debounced). The caller writes the durable
    thread regardless, so the ambient unread count stays truthful even when this
    nudge is debounced. Best-effort throughout: a notifier, events-write, or
    filesystem failure never breaks the send. Returns ``"escalated"`` (the human
    was notified), ``"debounced"`` (a recent nudge for this pair suppressed it),
    or ``"notifier-unavailable"`` (no OS notifier on this host, so nothing
    displayed - the caller must not claim escalation; the overlay event still
    fired).
    """

    from fno.paths import state_dir

    pair = hashlib.sha256(f"{sender}\x00{recipient}".encode()).hexdigest()[:16]
    # The debounce marker state machine lives behind the mail-receipt verb
    # (file budget): O_CREAT|O_EXCL claims the window atomically - exactly
    # one concurrent sender wins a fresh escalation, the rest see the marker
    # and debounce; a stale marker refreshes so the next window runs from now.
    try:
        verdict = _render(
            [
                "debounce",
                "--marker-dir", str(state_dir() / "mail-escalations"),
                "--pair", pair,
                "--window-secs", str(_ESCALATION_DEBOUNCE_S),
            ]
        )
    except Exception:  # noqa: BLE001 - a marker failure just re-notifies
        verdict = "claim"
    if verdict == "debounced":
        return "debounced"
    # Debounce gate passed: this is a real escalation. Emit the overlay event
    # BEFORE the notifier verdict - the overlay is an independent surface that
    # must render even on a headless host where the notifier is unavailable (the
    # whole point of surfacing in the mux). Best-effort: an events-write failure
    # never breaks the notifier or the send.
    try:
        from fno.events import append_event, mail_escalation

        append_event(
            mail_escalation(
                reason=reason,
                sender=sender,
                recipient=recipient,
                summary=summary.split("\n", 1)[0][:120],
                msg_id=msg_id,
            )
        )
    except Exception:  # noqa: BLE001 - an overlay miss never breaks the send
        pass
    # Only report escalation when the notification actually displayed:
    # send_notification returns (code, err) and a nonzero code means no OS
    # notifier (a headless host), so the human was NOT notified.
    try:
        from fno.notify._impl import send_notification

        one_line = summary.split("\n", 1)[0][:120]
        label = "missed you" if reason in ("attended-miss", "reachable-miss") else "question"
        code, _err = send_notification(
            f"fno agents mail: {label} from {sender}",
            f"{one_line} - run `fno agents mail drain-self`",
        )
    except Exception:  # noqa: BLE001 - a notifier failure never breaks the send
        code = 1
    return "escalated" if code == 0 else "notifier-unavailable"
