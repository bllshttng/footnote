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


#: The closers reconcile_channel mints. Mechanical, never a human verdict:
#: they do not suppress, so a set that changed away and back re-asks.
_MECHANICAL_CLOSERS = frozenset({"stale-escalate", "friction-escalate"})


def _is_answer_close(rec: dict, qids: "set[str]") -> bool:
    from fno.outstanding.core import QUESTION_CLOSED_EVENT

    data = rec.get("data")
    return (
        rec.get("type") == QUESTION_CLOSED_EVENT
        and isinstance(data, dict)
        and str(data.get("question_id") or "") in qids
        and bool(data.get("answer"))
    )


def _is_reset(rec: dict, marker: str) -> bool:
    data = rec.get("data")
    return (
        rec.get("type") == "operator_decision"
        and isinstance(data, dict)
        and str(data.get("subject") or "") == f"{marker}:reset"
    )


def answered_question(root: Path, key: str, *, marker: str = MARKER) -> "str | None":
    """The id of the question carrying ``[<marker>:<key>]`` that a human
    answered and no empty-set reset has retired, else None. Contract:
    docs/architecture/fleet-watchdog.md."""
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
    """Record the empty-set episode boundary for a marker, lazily: only when
    an answer is pending reset. The returning set then asks fresh."""
    import secrets

    from fno.events import operator_decision
    from fno.outstanding.core import append_question_event, read_answered_questions, read_question_events

    events = read_question_events()
    answered_ids = {q["id"] for q in read_answered_questions()
                    if f"[{marker}:" in q.get("question", "")}
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
