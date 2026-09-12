"""The shared question fold for every durable ``[<marker>:<key>]`` emitter.

``already_asked`` / ``answered_question`` / ``reset_answered`` and the
``reconcile_channel`` fold (moved here from ``stale_lane.py``) live in this
one module; the stale, friction, reap-hold, unfinished-work and
king-escalation lanes all ride them rather than growing private copies.
This module's own emitter is the unfinished-work escalation: a session row
no verb can clear is noise, and a durable question made of noise trains its
reader to ignore the channel. The ask names each finding identity and the
one command that clears it, deduped on outcome identity so a finding that
only aged does not re-ask.
"""
from __future__ import annotations

import hashlib
from pathlib import Path

MARKER = "watchdog-unfinished-work"
MAX_LISTED_ROWS = 10

#: Severity order for the question's rows and its ask line: the same order
#: the report's digest uses, so "clear the top finding first" names the
#: finding the digest lists first, not whichever sorts alphabetically.
from fno.agents.unfinished_work import DIMENSIONS as _DIMENSIONS  # noqa: E402

_SEVERITY = {kind: i for i, kind in enumerate(_DIMENSIONS)}


def dedupe_key(identities: "list[str]") -> str:
    joined = "\n".join(sorted(set(identities)))
    return hashlib.sha256(joined.encode("utf-8")).hexdigest()[:12]


def _unique(findings):
    return sorted(
        {f"{f.kind}:{f.subject}": f for f in findings}.values(),
        key=lambda f: (_SEVERITY.get(f.kind, len(_SEVERITY)), f.subject),
    )


def question_text(findings, key: str, unknown_dimensions=()) -> str:
    unique = _unique(findings)
    shown = [
        f"{f.kind} {f.subject}: {f.basis} -> clear: {f.clear_command}"
        for f in unique[:MAX_LISTED_ROWS]
    ]
    if len(unique) > MAX_LISTED_ROWS:
        shown.append(f"and {len(unique) - MAX_LISTED_ROWS} more")
    # An incomplete scan escalates what it DID reach and names what it did
    # not. Withholding the whole ask instead made the sweep permanently mute
    # wherever a deleted worktree root can never be fetched again.
    caveat = ""
    if unknown_dimensions:
        caveat = (
            f" The scan was INCOMPLETE - {', '.join(sorted(unknown_dimensions))} "
            f"went unread, so more findings may exist."
        )
    return (
        f"[{MARKER}:{key}] The fleet watchdog found {len(unique)} "
        f"unfinished-work finding(s). Each names the one command that clears "
        f"it.{caveat} Findings: {'; '.join(shown)}"
    )


def _ask_line(findings) -> str:
    return _unique(findings)[0].clear_command


def already_asked(root: Path, key: str, *, marker: str = MARKER) -> "str | None":
    """The id of the open question carrying ``[<marker>:<key>]``, else None.

    One shared dedupe fold for every durable question emitter: a marker key
    asked once stays asked, and a second emitter with its own marker reuses
    this fold rather than growing a second copy of it.
    """
    from fno.outstanding.core import read_open_questions

    needle = f"[{marker}:{key}]"
    for question in read_open_questions(root):
        if needle in question.question:
            return question.id
    return None


#: Closers reconcile_channel mints. Mechanical, never a human verdict.
_MECHANICAL_CLOSERS = frozenset({
    "stale-escalate",
    "friction-escalate",
    "reap-hold-escalate",
    "unfinished-work-escalate",
    "king-escalation-escalate",
})

#: Families whose key is a SNAPSHOT of a measured set, so the newest reading
#: supersedes every older one. A family whose key is an IDENTITY is not here:
#: session-transition-branch keys on name:predecessor:successor and king-wake
#: keys on the crown scope, where two open rows are two different questions.
SNAPSHOT_MARKERS = frozenset({
    "king-escalation",
    "watchdog-unfinished-work",
    "watchdog-stale",
    "reap-hold",
})


def _is_answer_close(rec: dict, qids: "set[str]") -> bool:
    from fno.outstanding.core import QUESTION_CLOSED_EVENT

    data = rec.get("data")
    return (rec.get("type") == QUESTION_CLOSED_EVENT and isinstance(data, dict)
            and str(data.get("question_id") or "") in qids and bool(data.get("answer")))


def _is_reset(rec: dict, marker: str) -> bool:
    data = rec.get("data")
    return (
        rec.get("type") == "operator_decision"
        and isinstance(data, dict)
        and str(data.get("subject") or "") == f"{marker}:reset"
    )


def answered_question(root: Path, key: str, *, marker: str = MARKER) -> "str | None":
    """The id of the question carrying ``[<marker>:<key>]`` that a human
    answered and no empty-set reset retired, else None."""
    from fno.outstanding.core import read_answered_questions, read_question_events

    needle = f"[{marker}:{key}]"
    hit = next((q for q in read_answered_questions()
                if needle in q.get("question", "")
                and q.get("closed_by") not in _MECHANICAL_CLOSERS), None)
    if hit is None:
        return None
    events = read_question_events()
    answer_idx = max(
        (i for i, rec in enumerate(events) if _is_answer_close(rec, {hit["id"]})),
        default=-1,
    )
    if answer_idx < 0 or any(_is_reset(rec, marker) for rec in events[answer_idx + 1 :]):
        return None
    return hit["id"]


def reset_answered(root: Path, *, marker: str) -> None:
    """Record the empty-set episode boundary, lazily: only when an answer is pending reset."""
    import secrets

    from fno.events import operator_decision
    from fno.outstanding.core import append_question_event, read_answered_questions, read_question_events

    events = read_question_events()
    answered_ids = {q["id"] for q in read_answered_questions() if f"[{marker}:" in q.get("question", "")}
    last = max(
        (i for i, rec in enumerate(events) if _is_answer_close(rec, answered_ids)),
        default=-1,
    )
    if last < 0 or any(_is_reset(rec, marker) for rec in events[last + 1 :]):
        return
    reset_id = f"d-{secrets.token_hex(4)}"
    append_question_event(
        operator_decision(
            decision_id=reset_id, question_id=reset_id,
            decision="measured set empty; answer suppression resets",
            subject=f"{marker}:reset", decided_by="fno agents question-fold",
            origin="scheduler", authority_source="agent",
            rationale="episode boundary, not an operator ruling", source="daemon",
        ),
        root,
    )


def _open_questions(root: Path, marker: str):
    from fno.outstanding.core import read_open_questions

    return [
        q for q in read_open_questions(root)
        if f"[{marker}:" in q.question
    ]


def _close_question(qid: str, answer: str, root: Path, *, lane: str = "stale") -> None:
    """Close one ask AND record the decision the close made (the stop gate
    holds a closed-with-answer question with no ``operator_decision``
    record): a mechanical supersede, never an operator ruling. ``lane`` is
    the channel short name, so closes record channel provenance."""
    import secrets

    from fno.events import operator_decision, operator_question_closed
    from fno.outstanding.core import append_question_event

    append_question_event(
        operator_question_closed(
            question_id=qid,
            answer=answer,
            closed_by=f"{lane}-escalate",
            source="daemon",
        ),
        root,
    )
    append_question_event(
        operator_decision(
            decision_id=f"d-{secrets.token_hex(4)}",
            decision=answer,
            subject=f"watchdog-{lane}:{qid}",
            question_id=qid,
            decided_by=f"fno agents {lane}-escalate",
            origin="scheduler",
            authority_source="agent",
            rationale="mechanical supersede by reconcile; not an operator ruling",
            source="daemon",
        ),
        root,
    )


def _try_close_supersede(q, reason: str, root: Path, subject: str) -> None:
    """Best-effort supersede close: the new ask is already durable, so a
    failing close must cost a duplicate ask, never the recorded one (AC3)."""
    try:
        _close_question(q.id, reason, root, lane=subject)
    except Exception:  # noqa: BLE001 - see docstring; supersede closes are cleanup
        pass


def reconcile_channel(
    pairs, *, root: Path, session_id: "str | None", cwd: Path,
    marker: str, subject: str, identities: "list[str]",
    question, ask, asker: "str | None" = None,
) -> "tuple[str, str]":
    """Reconcile ONE durable ``[<marker>:<key>]`` operator question to the
    measured ``pairs``: same set is a duplicate, a changed set supersedes,
    an empty set closes, and a set a human already answered stays answered.
    ``question``/``ask`` are callables taking the dedupe ``key``; outcome in
    ``none | duplicate | answered | asked | closed``."""
    key = dedupe_key(identities)

    if not pairs:
        open_qs = _open_questions(root, marker)
        for q in open_qs:
            _close_question(
                q.id, f"no {subject} rows remain at reconcile time", root,
                lane=subject,
            )
        reset_answered(root, marker=marker)
        return ("closed", open_qs[0].id) if open_qs else ("none", "")

    existing = already_asked(root, key, marker=marker)
    if existing:
        for q in _open_questions(root, marker):
            if q.id != existing:
                _try_close_supersede(q, f"{subject} set changed; superseded by {existing}",
                                     root, subject)
        return ("duplicate", existing)
    answered = answered_question(root, key, marker=marker)
    if answered:
        for q in _open_questions(root, marker):
            if q.id != answered:
                _try_close_supersede(q, f"{subject} set changed; superseded by {answered}",
                                     root, subject)
        return ("answered", answered)

    import secrets

    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    qid = f"q-{secrets.token_hex(4)}"
    # Append BEFORE closing superseded asks: a failed close must cost a duplicate ask, never an empty channel.
    append_question_event(
        operator_question(
            question_id=qid,
            question=question(key),
            session_id=session_id,
            cwd=str(cwd),
            asker=asker,
            ask=ask(key),
            source="daemon",
        ),
        root,
    )
    for q in _open_questions(root, marker):
        if q.id != qid:
            _try_close_supersede(q, f"{subject} set changed; superseded by {qid}",
                                 root, subject)
    return ("asked", qid)


def escalate_unfinished(
    findings,
    *,
    root: Path,
    session_id: "str | None",
    cwd: Path,
    unknown_dimensions=(),
) -> "tuple[str, str]":
    if not findings:
        outcome, qid = reconcile_channel(
            [],
            root=root,
            session_id=session_id,
            cwd=cwd,
            marker=MARKER,
            subject="unfinished-work",
            identities=[],
            question=lambda _key: "",
            ask=lambda _key: "",
        )
        return ("none", "") if outcome == "none" else (outcome, qid)

    unique = _unique(findings)
    outcome, qid = reconcile_channel(
        unique,
        root=root,
        session_id=session_id,
        cwd=cwd,
        marker=MARKER,
        subject="unfinished-work",
        identities=[f"{f.kind}:{f.subject}" for f in unique],
        question=lambda key: question_text(unique, key, unknown_dimensions),
        ask=lambda _key: f"clear the top finding first: {_ask_line(unique)}",
    )
    return ("recorded", qid) if outcome == "asked" else (outcome, qid)
