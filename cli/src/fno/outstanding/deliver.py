"""Deliver a closed answer to the asker over the mail bus.

Why this exists: ``clear --answer`` recorded a decision and closed the question
for a year, and the asking agent never learned any of it. The only consumers of
``operator_question_closed`` fold counts and gate loops; neither delivers. An
answer that reaches nobody trains producers to stop asking through the queue,
which is how the operator inbox died.

Every outcome is a stated posture line. Nothing here raises into ``clear``: the
close is durable before delivery runs, so delivery is best-effort with a name
for each way it cannot happen.
"""
from __future__ import annotations

import subprocess
from typing import Any, Optional

#: Bypass reason for the mail style gate. The body carries the operator's
#: verbatim answer, and the style gate judges authored relay prose; quoted
#: decision text is data passing through, not writing the sender did.
STYLE_EXCEPTION = "operator answer verbatim: quoted decision text, not authored prose"

_MAIL_TIMEOUT_SECONDS = 30


def _resolve_asker(asker: str) -> "tuple[Optional[Any], list[str]]":
    """The same resolver question liveness uses, so delivery and ranking agree."""
    from fno.agents.discover import resolve_reachable

    return resolve_reachable(asker)


def _mail_send(argv: "list[str]") -> "tuple[int, str]":
    """Run one ``fno agents mail send``. Every failure arrives as (code, detail)."""
    try:
        proc = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=_MAIL_TIMEOUT_SECONDS,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return (124, f"mail send timed out after {_MAIL_TIMEOUT_SECONDS}s")
    except OSError as exc:
        return (127, str(exc))
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip().splitlines()
        return (proc.returncode, detail[0] if detail else "no detail")
    # The verb's own evidence line ("delivered (hosted)", "queued (durable)")
    # is the delivery truth; exit 0 alone cannot tell landed from parked.
    out = (proc.stdout or "").strip().splitlines()
    return (0, out[-1] if out else "")


def _fno_argv() -> "list[str]":
    """The self-shellout prefix; a bare ``fno`` fails on a cargo-only install."""
    from fno import _subprocess_util

    return _subprocess_util.fno_py_cmd()


def deliver_answer(
    question: Any,
    answer: str,
    decision_id: "str | None",
) -> str:
    """Return the posture line for one answered close. Never raises.

    ``clear`` calls this once per closed id OUTSIDE its own try/except, so the
    never-raises contract is load-bearing: one unexpected resolver exception
    here would abort a multi-id clear mid-loop with some ids closed and no
    count printed. Every failure, expected or not, is a stated line.
    """
    try:
        return _deliver_answer(question, answer, decision_id)
    except Exception as exc:  # noqa: BLE001 - the close is durable; say what broke
        qid = getattr(question, "id", "?")
        did = decision_id or "the decision record"
        return (
            f"outstanding: {qid} answered; delivery failed ({exc}). "
            f"Decision {did}; recover it via: fno backlog decisions"
        )


def _deliver_answer(
    question: Any,
    answer: str,
    decision_id: "str | None",
) -> str:
    qid = question.id
    did = decision_id or "the decision record"
    asker = getattr(question, "asker", None)
    if not asker:
        return (
            f"outstanding: {qid} answered; no asker on record (king escalation), "
            f"nobody to wake. Decision {did}; recover it via: fno backlog decisions"
        )

    # Resolve by the FULL session id first, falling back to the 8-char handle.
    # Codex ids are time-prefixed so their first-8 collides across same-window
    # sessions; the full id was in hand at ask time (it is the question's own
    # session_id) and is unique by construction, so the collision case only
    # exists for rows stamped before this ordering.
    token = getattr(question, "session_id", None) or asker
    session, ambiguous = _resolve_asker(token)
    if ambiguous:
        return (
            f"outstanding: {qid} answered; asker {asker} is ambiguous across "
            f"{len(ambiguous)} stored sessions, not delivered (never guess). "
            f"Decision {did}"
        )
    if session is None:
        return (
            f"outstanding: {qid} answered; asker {asker} has no stored session, "
            f"nobody to wake. Decision {did}; recover it via: fno backlog decisions"
        )

    full_id = getattr(session, "session_id", None) or token
    body = f'Answer to your question {qid} "{question.question}": {answer}.'
    argv = [
        *_fno_argv(),
        "mail",
        "send",
        full_id,
        body,
        "--style-exception",
        STYLE_EXCEPTION,
    ]
    code, detail = _mail_send(argv)
    if code != 0:
        return (
            f"outstanding: {qid} answered; delivery failed ({detail}). "
            f'Retry: fno agents mail send {full_id} "<the answer>" '
            f'--style-exception "operator answer verbatim"'
        )
    # Lead with the bus's own evidence, never a verdict word of ours: exit 0
    # covers both "delivered (hosted)" and "queued (durable)", and a durable
    # park under a session nobody resumes is NOT delivered no matter what the
    # prefix claimed. The evidence line names which happened.
    evidence = f": {detail}" if detail else ""
    return f"outstanding: {qid} answered; mail to {asker}{evidence} (decision {did})"
