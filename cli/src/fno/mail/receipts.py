"""What a durable mail receipt says about the recipient's reachability.

Extracted from ``fno.mail.cli`` (file-budget: that module is shrink-only) so
the demotion-receipt logic has a home named by the question it answers: the
stderr recovery warning every durable floor prints, its live-lane reason
vocabulary, and the transcript-age suffix a bare live-miss carries.
"""

from __future__ import annotations

import sys
from typing import Optional


def _live_miss_age_suffix(recipient: str) -> str:
    """The transcript-age suffix a bare live-miss receipt carries (AC8).

    A bare live-miss reads the same for a transient miss to a genuinely live
    peer (re-send works) and for a session that stood down hours ago (nothing
    will drain it); the age is the discriminator. An unreadable transcript
    prints unknown, never 0s. Three lanes emit that receipt (the name lane,
    the job lane, the registered-agent lane), so the suffix lives here once.
    """
    from fno.agents.session_truth import resolve_session_truth
    from fno.agents.top import _fmt_age

    age_s = resolve_session_truth(recipient).get("last_activity_age_s")
    if age_s is None:
        return ", transcript age unknown"
    return f", transcript quiet {_fmt_age(age_s)}"


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
    if hours <= 0:
        return ""
    if hours < 1:
        window = f"~{round(hours * 60)}m"
    else:
        window = f"~{round(hours)}h"
    return f" - typically drains within {window} - an empty unread before then is not a failure"


def durable_leg_story(reason: Optional[str]) -> Optional[str]:
    """Positive stdout wording for a live-lane failure demotion : a
    live-inject miss plus a durable success is a normal outcome, and the raw
    token (``io-error``, ``attach-failed``, ...) rendered an error string
    inside a success receipt. None when the reason is not a live-lane
    failure; the token stays diagnostic (stderr advisory, bus record)."""
    if not _is_live_lane_failure(reason):
        return None
    return "live leg unconfirmed; durable leg holds"


def demotion_receipt(
    msg_id: str,
    *,
    reason: Optional[str],
    owner: Optional[str],
    target: Optional[str] = None,
    project: Optional[str] = None,
    age_target: Optional[str] = None,
) -> str:
    """The stdout line for a durable demotion (refined by x-aaaa)."""
    token = durable_leg_story(reason)
    if token is None:
        token = reason or "live-miss"
        if token == "live-miss" or token.startswith("transcript-"):
            age_of = age_target if age_target is not None else target
            if age_of is not None:
                token += _live_miss_age_suffix(age_of)
    where = f" for {target}" if target else ""
    if project:
        where += f" [project {project}]"
    return f"{msg_id} queued (durable){where} [{token}]" + durable_window_clause(owner)


def print_project_demotion(result, to_project: str) -> None:
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
            print(
                f"{result.msg_id} queued (durable) for {result.recipient} "
                f"[project {to_project}] "
                f"[{_note or 'DND (bus-only): recipient polls the bus at each turn boundary'}]"
                + (f" `fno agents mail withdraw {result.msg_id}` retracts it." if _note else "")
                + durable_window_clause(result.durable_owner)
            )
        else:
            _warn_deferred(result.recipient, reason=result.reason)
            print(demotion_receipt(
                result.msg_id,
                reason=result.reason, owner=result.durable_owner,
                target=result.recipient, project=to_project,
            ))
        return
    from fno.inbox.store import DurableOwner

    _warn_deferred(to_project, project=True)
    print(
        f"{result.msg_id} queued (durable) for project {to_project} "
        f"[param-forced: --to-project]"
        + durable_window_clause(DurableOwner.INBOX_DRAIN.value)
    )


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

    if project:
        msg = (
            f"mail: project inbox {target} has no live drain; queued durably as "
            "recovery only - a session must drain the project inbox to read this, "
            "and may never do so\n"
            "  this is NOT delivery. Address a live session instead: "
            "`fno agents top` to find one, then `fno agents mail send <short-id>`"
        )
    elif reason == LOCK_TIMEOUT_REASON:
        msg = (
            f"mail: live delivery to {target} was not attempted (another verb "
            f"held {target}'s agent lock past the wait); queued durably. That "
            "holder is any verb on this agent - a send, an ask, a spawn, a "
            "stop, an rm - so the token proves nothing about the recipient in "
            "either direction. Do not resurrect it on this evidence, and do "
            "not read it as healthy either: check it.\n"
            "  a busy peer may not drain soon, so the rungs that stay open,\n"
            "  in this order - a bare re-send DOUBLE-DELIVERS, since the queued\n"
            "  copy still lands at the recipient's next drain:\n"
            f"    fno agents peek {target}     # still taking turns, or just stopped?\n"
            "    fno agents mail withdraw <id>      # retract the queued copy FIRST\n"
            f"    fno agents mail send {target} '<message>'  # then retry live\n"
            "  a withdraw that refuses because the recipient already claimed\n"
            "  the message is telling you it LANDED. Stop there: re-sending on\n"
            "  top of that is the double delivery this ladder exists to avoid."
        )
    elif _is_live_lane_failure(reason):
        msg = (
            f"mail: live delivery to {target} not confirmed ({reason}); queued "
            "durably as recovery only - the recipient was live and reachable, so "
            "the message may still land past the confirm window or sit until the "
            "recipient drains its inbox\n"
            "  live delivery NOT confirmed - do not wait for a reply, recover:\n"
            f"    fno agents peek {target}     # did it land? a busy peer may have queued it\n"
            f"    fno agents resume {target}   # wakes it (claude) or resumes it (other harnesses), then re-send\n"
            f"    fno agents attach {target}   # drive it yourself (claude)\n"
            # The rung that was missing. Every option above tries to reach the
            # recipient; when none of them can, the sender was left holding a
            # message that nagged every turn and could not be taken back.
            "    fno agents mail withdraw <id>      # none of the above? retract it"
        )
    else:
        from fno.mail.deferred_liveness import deferred_liveness_head
        msg = deferred_liveness_head(target) + (
            "  live delivery NOT confirmed - do not wait for a reply, recover:\n"
            f"    fno agents peek {target}     # did it land? a busy peer may have queued it\n"
            f"    fno agents resume {target}   # wakes it (claude) or resumes it (other harnesses), then re-send\n"
            f"    fno agents attach {target}   # drive it yourself (claude)\n"
            # The rung that was missing. Every option above tries to reach the
            # recipient; when none of them can, the sender was left holding a
            # message that nagged every turn and could not be taken back.
            "    fno agents mail withdraw <id>      # none of the above? retract it"
        )
    print(msg, file=sys.stderr)

