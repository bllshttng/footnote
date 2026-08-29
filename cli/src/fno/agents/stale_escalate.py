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


def escalate_unfinished(
    findings,
    *,
    root: Path,
    session_id: "str | None",
    cwd: Path,
) -> "tuple[str, str]":
    if not findings:
        return ("none", "")

    import secrets

    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    unique = _unique(findings)
    key = dedupe_key([f"{f.kind}:{f.subject}" for f in unique])
    existing = already_asked(root, key)
    if existing:
        return ("duplicate", existing)

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
