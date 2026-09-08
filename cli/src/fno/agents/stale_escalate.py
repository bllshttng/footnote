"""Escalate the watchdog's unfinished-work findings to one durable operator question.

Replaces the stale-session question: a session row no verb can clear is
noise, and a durable question made of noise trains its reader to ignore the
channel. The ask now names each finding identity and the one command that
clears it, deduped on outcome identity so a finding that only aged does not
re-ask.
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


def question_text(findings, key: str) -> str:
    unique = _unique(findings)
    shown = [
        f"{f.kind} {f.subject}: {f.basis} -> clear: {f.clear_command}"
        for f in unique[:MAX_LISTED_ROWS]
    ]
    if len(unique) > MAX_LISTED_ROWS:
        shown.append(f"and {len(unique) - MAX_LISTED_ROWS} more")
    return (
        f"[{MARKER}:{key}] The fleet watchdog found {len(unique)} "
        f"unfinished-work finding(s). Each names the one command that clears "
        f"it. Findings: {'; '.join(shown)}"
    )


def _ask_line(findings) -> str:
    unique = _unique(findings)
    if not unique:
        return "fno agents watchdog"
    first = unique[0]
    return first.clear_command


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


#: The closers reconcile_channel itself mints. Their close answers are
#: mechanical ("set changed; superseded by ..."), never a human verdict, so
#: they do not suppress: a set that changed away and back must re-ask.
_MECHANICAL_CLOSERS = frozenset({"stale-escalate", "friction-escalate"})


def answered_question(root: Path, key: str, *, marker: str = MARKER) -> "str | None":
    """The id of the question carrying ``[<marker>:<key>]`` that a HUMAN
    answered IN THIS EPISODE, else None. An answered ask is a consumed ask:
    re-minting it on the next sweep is the re-nag this fold kills. The
    suppression holds only until an empty-set reset - the episode boundary
    that makes a returning identity new work. Mechanical supersede closes
    are not answers and never suppress. Order is read from journal POSITION,
    never timestamps: second-granularity stamps tie.
    """
    from fno.outstanding.core import (
        QUESTION_CLOSED_EVENT,
        read_answered_questions,
        read_question_events,
    )

    needle = f"[{marker}:{key}]"
    hit = next(
        (
            question
            for question in read_answered_questions()
            if needle in question.get("question", "")
            and question.get("closed_by") not in _MECHANICAL_CLOSERS
        ),
        None,
    )
    if hit is None:
        return None
    events = read_question_events()
    answer_idx = None
    for i, rec in enumerate(events):
        data = rec.get("data")
        if (
            rec.get("type") == QUESTION_CLOSED_EVENT
            and isinstance(data, dict)
            and str(data.get("question_id") or "") == hit["id"]
            and data.get("answer")
        ):
            answer_idx = i
    if answer_idx is None:
        return None  # no readable answer event: nothing anchors the episode
    for rec in events[answer_idx + 1 :]:
        data = rec.get("data")
        if (
            rec.get("type") == "operator_decision"
            and isinstance(data, dict)
            and str(data.get("subject") or "") == f"{marker}:reset"
        ):
            return None  # the set emptied after the answer: a new episode
    return hit["id"]


def reset_answered(root: Path, *, marker: str) -> None:
    """Record the episode boundary for a marker: the measured set emptied, so
    every prior answer stops suppressing and a returning set asks fresh. Not
    an operator ruling - a mechanical marker the fold reads back. Written
    only when an answer is actually pending reset, so a clean fleet adds no
    journal rows. Positional, never timestamp, ordering: stamps tie."""
    import secrets

    from fno.events import operator_decision
    from fno.outstanding.core import (
        QUESTION_CLOSED_EVENT,
        append_question_event,
        read_answered_questions,
        read_question_events,
    )

    needle = f"[{marker}:"
    answered_ids = {
        question["id"]
        for question in read_answered_questions()
        if needle in question.get("question", "")
    }
    events = read_question_events()
    last_answer_idx = -1
    for i, rec in enumerate(events):
        data = rec.get("data")
        if (
            rec.get("type") == QUESTION_CLOSED_EVENT
            and isinstance(data, dict)
            and str(data.get("question_id") or "") in answered_ids
            and data.get("answer")
        ):
            last_answer_idx = i
    if last_answer_idx == -1:
        return  # nothing is suppressing; there is nothing to reset
    for rec in events[last_answer_idx + 1 :]:
        data = rec.get("data")
        if (
            rec.get("type") == "operator_decision"
            and isinstance(data, dict)
            and str(data.get("subject") or "") == f"{marker}:reset"
        ):
            return  # already reset after the newest answer
    reset_id = f"d-{secrets.token_hex(4)}"
    append_question_event(
        operator_decision(
            decision_id=reset_id,
            question_id=reset_id,
            decision="measured set empty; answer suppression resets",
            subject=f"{marker}:reset",
            decided_by="fno agents question-fold",
            origin="scheduler",
            authority_source="agent",
            rationale="episode boundary, not an operator ruling",
            source="daemon",
        ),
        root,
    )


def escalate_unfinished(
    findings,
    *,
    root: Path,
    session_id: "str | None",
    cwd: Path,
) -> "tuple[str, str]":
    if not findings:
        reset_answered(root, marker=MARKER)
        return ("none", "")

    import secrets

    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    unique = _unique(findings)
    key = dedupe_key([f"{f.kind}:{f.subject}" for f in unique])
    existing = already_asked(root, key)
    if existing:
        return ("duplicate", existing)
    answered = answered_question(root, key)
    if answered:
        return ("answered", answered)

    qid = f"q-{secrets.token_hex(4)}"
    append_question_event(
        operator_question(
            question_id=qid,
            question=question_text(unique, key),
            session_id=session_id,
            cwd=str(cwd),
            ask=f"clear the top finding first: {_ask_line(unique)}",
            source="daemon",
        ),
        root,
    )
    return ("recorded", qid)
