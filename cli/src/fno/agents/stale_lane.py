"""The stale-row question lane: reconcile one ``[watchdog-stale:*]`` operator
question to the set the sweep actually measured.

A row past the wake ceiling is the watchdog's needs-human bucket - no action
lane may take it, so the only honest surface is a human's. This module owns
that punt. It is deliberately NOT the report path: the AC9 census
(``test_ac9_census_no_session_predicates_on_the_report_path``) guards
``stale_escalate.py`` against session-bookkeeping vocabulary, because PR 1227
measured the stale ask as noise when it rode the unfinished-work channel. The
separation is the design: the report path carries findings a verb clears;
this lane carries rows only a human clears.

Deliberately the SAME marker the retired emitter used (commit e5acde858):
q-1b3c646b and its siblings are this channel's live history, and a new marker
would strand them as a second, unfed lane beside this one.
"""
from __future__ import annotations

import re
from pathlib import Path

from fno.agents.stale_escalate import already_asked, dedupe_key

STALE_MARKER = "watchdog-stale"

#: ``(\d+)h old`` - the age phrase both STALE verdict bases carry.
_AGE_H_RE = re.compile(r"(\d+)h old")


def oldest_h(bases: "list[str]") -> "int | None":
    ages = [int(m.group(1)) for b in bases for m in (_AGE_H_RE.search(b),) if m]
    return max(ages) if ages else None


def _open_stale_questions(root: Path):
    from fno.outstanding.core import read_open_questions

    return [
        q for q in read_open_questions(root)
        if f"[{STALE_MARKER}:" in q.question
    ]


def _close_question(qid: str, answer: str, root: Path) -> None:
    from fno.events import operator_question_closed
    from fno.outstanding.core import append_question_event

    append_question_event(
        operator_question_closed(
            question_id=qid,
            answer=answer,
            closed_by="stale-escalate",
            source="daemon",
        ),
        root,
    )


def reconcile_stale(stale_pairs, *, root: Path, session_id: "str | None",
                    cwd: Path) -> "tuple[str, str]":
    """Reconcile the durable stale-row question to the measured set.

    ``stale_pairs`` is (Verdict, Row) pairs the caller filtered out of a real
    ``run_sweep`` - the fold never classifies, it only reconciles the channel
    to what the sweep measured. One open question per identity set: same set
    is a duplicate, a changed set closes the stale-keyed asks and asks fresh,
    an empty set closes what is open.

    Returns ``(outcome, question_id)`` with outcome in ``none | duplicate |
    asked | closed``; the id is the asked question, else the first closed one.
    """
    key = dedupe_key([f"stale:{v.row_id}" for v, _row in stale_pairs])

    if not stale_pairs:
        open_qs = _open_stale_questions(root)
        for q in open_qs:
            _close_question(q.id, "no stale rows remain at reconcile time", root)
        return ("closed", open_qs[0].id) if open_qs else ("none", "")

    existing = already_asked(root, key, marker=STALE_MARKER)
    if existing:
        # Hygiene on the repeat visit: a previous run that appended its ask
        # but died mid-close leaves superseded asks open. Closing them here
        # keeps one-open-ask-per-set true without re-asking.
        for q in _open_stale_questions(root):
            if q.id != existing:
                _close_question(
                    q.id, f"stale set changed; superseded by {existing}", root
                )
        return ("duplicate", existing)

    import secrets

    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    qid = f"q-{secrets.token_hex(4)}"
    shown = [
        f"{v.name} [node {_row.node or 'unknown'}]: {v.basis}"
        for v, _row in stale_pairs
    ]
    oldest = oldest_h([v.basis or "" for v, _row in stale_pairs])
    age_clause = f", oldest {oldest}h" if oldest is not None else ""
    question = (
        f"[{STALE_MARKER}:{key}] The fleet watchdog holds {len(stale_pairs)} "
        f"stale row(s) no lane will act on{age_clause}. Nothing in the sweep "
        "clears these; each needs a human to reap it or resume it. Rows: "
        + "; ".join(shown)
    )
    ask = (
        f"triage {len(stale_pairs)} stale watchdog row(s){age_clause}: "
        "fno agents watchdog --only stale"
    )
    # Append the replacement BEFORE closing the superseded asks: a failed
    # close must cost a duplicate ask, never an empty channel.
    append_question_event(
        operator_question(
            question_id=qid,
            question=question,
            session_id=session_id,
            cwd=str(cwd),
            ask=ask,
            source="daemon",
        ),
        root,
    )
    for q in _open_stale_questions(root):
        if q.id == qid:
            continue
        _close_question(q.id, f"stale set changed; superseded by {qid}", root)
    return ("asked", qid)
