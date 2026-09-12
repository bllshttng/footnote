"""Escalate a stalled king board to the operator, exactly once per stalled set.

A king terminating ``NoProgress`` exits quietly: work pending, nothing moving,
nobody told. The escalation is the telling, a question in the operator queue
because the queue survives the next turn. Idempotence keys on the stalled id
SET - a respawned king meeting the same board records no second question,
while a different board is the SAME ask, re-measured: the channel supersedes
the stale row and asks once on the new reading. The board churns; the
question does not change.
"""
from __future__ import annotations

from pathlib import Path

MARKER = "king-escalation"


# How many stalled rows the question names before it says "and N more". The
# rows are context; the COUNT and the key are the load-bearing parts, and a
# board with a hundred stalled rows must not push either out of the text.
MAX_LISTED_IDS = 20


def _stalled_subject(stalled_ids: "list[str]") -> str:
    ids = sorted(set(stalled_ids))
    if not ids:
        return "a board the king could not read"
    shown = ", ".join(ids[:MAX_LISTED_IDS])
    if len(ids) > MAX_LISTED_IDS:
        shown += f", and {len(ids) - MAX_LISTED_IDS} more"
    return f"{len(ids)} board row(s) nothing is clearing: {shown}"


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

    ``live`` branches the closing sentence on the CALLER's measured liveness:
    telling the operator a live king "has exited" hands it the double-crown
    recommendation. ``None`` (unreadable) reads as dead, naming the reason.
    """
    subject = _stalled_subject(stalled_ids)
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


def escalate(stalled_ids: "list[str]", reason: str, root: Path, session_id: "str | None",
             cwd: Path, *, live: "bool | None" = None,
             unknown_reason: "str | None" = None) -> "tuple[str, str]":
    """Record one operator question for this stalled set.

    Returns ``(outcome, question_id)`` where outcome is ``recorded``,
    ``duplicate``, ``answered`` or ``closed``. Raises on a store failure; a
    quiet failure here would put the king back in the silence this verb exists
    to break. ``live`` and ``unknown_reason`` come from
    :func:`fno.king.state.reign_state`; the dedupe key is unchanged either way.
    """
    from fno.agents.stale_escalate import reconcile_channel
    from fno.harness_identity import canonical_handle

    ids = sorted(set(stalled_ids))
    # An empty stalled list is an UNREADABLE board, never a clean one - the
    # question must say so (test_an_empty_stalled_set_never_reads_as_a_clean_board).
    # The channel's empty branch closes, which would read as clean, so hand it
    # a stable sentinel identity to ask under instead.
    pairs = ids or ["unreadable"]
    outcome, qid = reconcile_channel(
        pairs,
        root=root,
        session_id=session_id,
        cwd=cwd,
        marker=MARKER,
        subject="king-escalation",
        identities=ids if ids else ["king-board-unreadable"],
        question=lambda key: question_text(
            ids, key, reason, live=live, unknown_reason=unknown_reason
        ),
        # No ask line. A stalled row is queue-qualified (`undispatched:x-1234`)
        # and not every queue holds backlog nodes, so any single clearing
        # command here would be a guess. The question text names the rows.
        ask=lambda _key: "",
        # The delivery address for the eventual answer. The king that asked
        # is dead by then, but the durable mail tier reaches its successor;
        # an asker-less row can only ever be answered into the void.
        asker=canonical_handle(session_id) if session_id else None,
    )
    return ("recorded", qid) if outcome == "asked" else (outcome, qid)


def resolve_presiding_king(session_id: "str | None") -> "dict | None":
    """The crown above the escalating session's own, or None (x-3ecf AC4-HP)."""
    if not session_id:
        return None
    try:
        from fno.agents.court import find_presiding_crown, gather_court
        from fno.agents.crown import _graph_index
        from fno.agents.registry import load_registry
        from fno.harness_identity import session_identity_key

        needle = session_identity_key(session_id)

        def _keyed(r: object) -> "str | None":
            sid = getattr(r, "harness_session_id", None)
            return session_identity_key(sid) if sid else None

        own = next((r for r in load_registry() if _keyed(r) == needle), None)
        if own is None or own.crown_level is None or not own.crown_scope:
            return None
        crowns = gather_court().get("crowns")
        if not crowns:
            return None
        return find_presiding_crown(own.crown_scope, own.crown_level, crowns, _graph_index())
    except Exception:  # noqa: BLE001 - a read failure falls through to the operator
        return None


def mail_presiding_king(holder: str, stalled_ids: "list[str]", reason: str) -> bool:
    """True only on a confirmed send; any failure reads False."""
    import shutil
    import subprocess

    fno_bin = shutil.which("fno")
    if not fno_bin:
        return False
    subject = _stalled_subject(stalled_ids)
    message = (
        f"A crown under yours stopped on {subject}. Reason given: {reason}. "
        "It presides over territory yours contains - check on it before this reaches the operator."
    )
    try:
        proc = subprocess.run(
            [fno_bin, "agents", "mail", "send", holder, message],
            capture_output=True, timeout=15, check=False,
        )
        return proc.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False
