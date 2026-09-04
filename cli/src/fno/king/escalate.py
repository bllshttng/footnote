"""Escalate a stalled king board to the operator, exactly once per stalled set.

A king that terminates ``NoProgress`` exits quietly, which is this feature's own
failure arriving through a different door: work is pending, nothing is moving,
and nobody is told. The escalation is the telling. It is a question in the
operator queue rather than a log line because the queue is what survives the
next turn.

Idempotence is keyed on the stalled id SET, not on the session or the fire: a
king respawned by the walk arm meets the same stalled board and must not record
a second question about it. Two different stalled sets are two different
questions, which is right - the board changed, so the ask changed.
"""
from __future__ import annotations

import hashlib
from pathlib import Path

MARKER = "king-escalation"


def dedupe_key(stalled_ids: "list[str]") -> str:
    """A stable short key for one stalled set, order-independent."""
    joined = "\n".join(sorted(set(stalled_ids)))
    return hashlib.sha256(joined.encode("utf-8")).hexdigest()[:12]


# How many stalled rows the question names before it says "and N more". The
# rows are context; the COUNT and the key are the load-bearing parts, and a
# board with a hundred stalled rows must not push either out of the text.
MAX_LISTED_IDS = 20


def question_text(
    stalled_ids: "list[str]",
    key: str,
    reason: str,
    *,
    live: "bool | None" = None,
    unknown_reason: "str | None" = None,
) -> str:
    """The operator-facing text, with the dedupe marker FIRST.

    The marker leads because ``operator_question`` truncates the recorded text
    at ``QUESTION_CAP``. With the marker last, a long enough id list pushed it
    past the cap, ``already_asked`` stopped matching, and every respawned king
    filed a fresh duplicate - the exact failure this module exists to prevent.
    Leading it also caps the id list, so neither half can crowd the other out.

    ``live`` branches the closing sentence on the CALLER's measured liveness
    (:func:`fno.king.state.reign_state`). A live king has not exited, and
    telling the operator it has - with "crown a new king" as a remedy - is the
    double-crown recommendation ``fno agents court`` exists to end. ``None``
    (liveness unreadable) reads as dead: a question that under-claims is safe,
    one that over-claims crowns a second king over a live scope.
    """
    ids = sorted(set(stalled_ids))
    if ids:
        shown = ", ".join(ids[:MAX_LISTED_IDS])
        if len(ids) > MAX_LISTED_IDS:
            shown += f", and {len(ids) - MAX_LISTED_IDS} more"
        subject = f"{len(ids)} board row(s) nothing is clearing: {shown}"
    else:
        subject = "a board the king could not read"
    if live:
        closing = (
            "It is still reigning and holding these rows, so decide whether to "
            "unblock them, defer them, or tell it to stand down."
        )
    else:
        closing = (
            "It has exited, so nothing restarts it on its own - decide whether "
            "to unblock these rows, defer them, or crown a new king."
        )
        if live is None and unknown_reason:
            # The unknown is named, never silently dropped: an operator told
            # only "it has exited" would not know the liveness read failed and
            # the king may in fact be live.
            closing += f" (liveness unreadable: {unknown_reason})"
    return (
        f"[{MARKER}:{key}] The king stopped on {subject}. "
        f"Reason given: {reason}. "
        f"{closing}"
    )


def already_asked(root: Path, key: str) -> "str | None":
    """The id of an open question already carrying this key, if there is one.

    A read failure is NOT treated as "nothing asked yet". It raises, and the
    caller reports the escalation as failed, because a reader that cannot tell
    "no prior question" from "cannot see prior questions" would file a fresh
    question on every fire.
    """
    from fno.outstanding.core import read_open_questions

    needle = f"[{MARKER}:{key}]"
    for question in read_open_questions(root):
        if needle in question.question:
            return question.id
    return None


def escalate(stalled_ids: "list[str]", reason: str, root: Path, session_id: "str | None",
             cwd: Path, *, live: "bool | None" = None,
             unknown_reason: "str | None" = None) -> "tuple[str, str]":
    """Record one operator question for this stalled set.

    Returns ``(outcome, question_id)`` where outcome is ``recorded`` or
    ``duplicate``. Raises on a store failure; a quiet failure here would put the
    king back in the silence this verb exists to break. ``live`` and
    ``unknown_reason`` come from :func:`fno.king.state.reign_state` resolved by
    the caller; the dedupe key is unchanged either way, so a live king and a
    dead one over the same stalled set still collapse to one question.
    """
    import secrets

    from fno.events import operator_question
    from fno.harness_identity import canonical_handle
    from fno.outstanding.core import append_question_event

    key = dedupe_key(stalled_ids)
    existing = already_asked(root, key)
    if existing:
        return ("duplicate", existing)

    ids = sorted(set(stalled_ids))
    qid = f"q-{secrets.token_hex(4)}"
    append_question_event(
        operator_question(
            question_id=qid,
            question=question_text(
                ids, key, reason, live=live, unknown_reason=unknown_reason
            ),
            session_id=session_id,
            cwd=str(cwd),
            # The delivery address for the eventual answer. The king that asked
            # is dead by then, but the durable mail tier reaches its successor;
            # an asker-less row can only ever be answered into the void.
            asker=canonical_handle(session_id) if session_id else None,
            # No node. A stalled row is queue-qualified (`undispatched:x-1234`)
            # and not every queue holds backlog nodes, so any value here would
            # be a guess. The question text names the rows instead.
        ),
        root,
    )
    return ("recorded", qid)
