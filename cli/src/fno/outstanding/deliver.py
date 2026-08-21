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
from typing import Any, Callable, Optional

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
    """Run one ``fno mail send``. Every failure arrives as (code, detail)."""
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
    return (0, "")


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

    ``question`` is a ``core.Question`` (duck-typed: ``id``, ``question``,
    ``asker``). The asker handle is the delivery address stamped at ask time;
    rows without one (king escalations predating the stamp) get a stated
    posture naming where the answer is recoverable instead of a silent drop.
    """
    qid = question.id
    did = decision_id or "the decision record"
    asker = getattr(question, "asker", None)
    if not asker:
        return (
            f"outstanding: {qid} answered; no asker on record (king escalation), "
            f"nobody to wake. Decision {did}; recover it via: fno backlog decisions"
        )

    session, ambiguous = _resolve_asker(asker)
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

    # Address by the FULL session id: the 8-char handle collides across
    # same-window codex sessions, and mail queued under the wrong key is
    # mail no drain ever reads.
    full_id = getattr(session, "session_id", None) or asker
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
            f'Retry: fno mail send {full_id} "<the answer>" '
            f'--style-exception "operator answer verbatim"'
        )
    return f"outstanding: {qid} answered; delivered to {asker} by mail (decision {did})"
