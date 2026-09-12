"""Escalate a stalled king board to the operator, exactly once per stalled set.

A king terminating ``NoProgress`` exits quietly: work pending, nothing moving,
nobody told. The escalation is the telling, a question in the operator queue
because the queue survives the next turn. Idempotence keys on the stalled id
SET - a respawned king meeting the same board records no second question, while
a different board is a different ask.
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
# count and the key are the load-bearing parts, the id list is context, and
# the ask gate caps the line at config.style.word_cap.ask words.
MAX_LISTED_IDS = 3


def _stalled_subject(stalled_ids: "list[str]") -> str:
    ids = sorted(set(stalled_ids))
    if not ids:
        return "a board the king could not read"
    shown = ", ".join(ids[:MAX_LISTED_IDS])
    if len(ids) > MAX_LISTED_IDS:
        shown += f", and {len(ids) - MAX_LISTED_IDS} more"
    return f"{len(ids)} board row(s): {shown}"


def question_text(
    stalled_ids: "list[str]",
    key: str,
    reason: str,
    *,
    live: "bool | None" = None,
    unknown_reason: "str | None" = None,
) -> str:
    """The operator-facing text: one line, dedupe marker first, ids capped.

    The marker leads because ``already_asked`` matches on it, and a question
    truncated past its marker dedupes into duplicates - the exact failure this
    module exists to prevent. ``unknown_reason`` is deliberately absent from
    the text: the ask gate caps the line, and the caller echoes the reason on
    stderr so the unknown is still named, never silently dropped.

    ``live`` branches the closing sentence on the CALLER's measured liveness:
    telling a live king "has exited" hands it the double-crown recommendation.
    ``None`` (unreadable) reads as dead, naming that the read failed.
    """
    subject = _stalled_subject(stalled_ids)
    if live:
        closing = "Unblock, defer, or stand it down?"
    else:
        closing = "Unblock, defer, or crown a new king?"
        if live is None:
            closing += " (liveness unreadable)"
    _ = unknown_reason
    return f"[{MARKER}:{key}] The king stopped on {subject}. Reason: {reason}. {closing}"


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


def escalate(
    stalled_ids: "list[str]",
    reason: str,
    root: Path,
    session_id: "str | None",
    cwd: Path,
    *,
    live: "bool | None" = None,
    unknown_reason: "str | None" = None,
    scope: "str | None" = None,
) -> "tuple[str, str]":
    """Record one operator question for this stalled set.

    Returns ``(outcome, question_id)`` where outcome is ``recorded`` or
    ``duplicate``. Raises on a store failure; a quiet failure here would put the
    king back in the silence this verb exists to break. ``live`` and
    ``unknown_reason`` come from :func:`fno.king.state.reign_state`; the dedupe
    key is unchanged either way.
    """
    import re
    import secrets

    from fno.events import operator_question
    from fno.graph._constants import NODE_ID_BODY
    from fno.harness_identity import canonical_handle
    from fno.outstanding.core import append_question_event

    key = dedupe_key(stalled_ids)
    existing = already_asked(root, key)
    if existing:
        return ("duplicate", existing)

    ids = sorted(set(stalled_ids))
    # The pointer rule, satisfied from the run's own facts: the reign scope is
    # the node (when it spells one), and every stalled id that CONTAINS a node
    # id contributes it (`stalled_holder:x-1005` gives `x-1005`). Shape only,
    # never existence.
    node = scope if scope and re.fullmatch(NODE_ID_BODY, scope) else None
    blocks = sorted(
        {
            match
            for raw in ids
            for match in re.findall(rf"\b{NODE_ID_BODY}\b", raw)
        }
    )
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
            node=node,
            blocks=blocks or None,
        ),
        root,
    )
    return ("recorded", qid)


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
