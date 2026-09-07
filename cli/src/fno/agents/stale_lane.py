"""The stale-row question lane: reconcile one ``[watchdog-stale:*]`` operator
question to the set the sweep actually measured. A row past the wake ceiling
is the needs-human bucket - no action lane may take it, so the only honest
surface is a human's. The generic ``reconcile_channel`` below serves every
report-only question lane (the friction lane rides it too). Deliberately NOT
the report path: the AC9 census guards ``stale_escalate.py`` against
session-bookkeeping vocabulary (PR 1227 measured the stale ask as noise when
it rode the unfinished-work channel).
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


def _open_questions(root: Path, marker: str):
    from fno.outstanding.core import read_open_questions

    return [
        q for q in read_open_questions(root)
        if f"[{marker}:" in q.question
    ]


def _close_question(qid: str, answer: str, root: Path) -> None:
    """Close one ask AND record the decision the close made (the stop gate
    holds a closed-with-answer question that carries no
    ``operator_decision`` record): a mechanical supersede, never an operator
    ruling."""
    import secrets

    from fno.events import operator_decision, operator_question_closed
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
    append_question_event(
        operator_decision(
            decision_id=f"d-{secrets.token_hex(4)}",
            decision=answer,
            subject=f"watchdog-stale:{qid}",
            question_id=qid,
            decided_by="fno agents stale-escalate",
            origin="scheduler",
            authority_source="agent",
            rationale="mechanical supersede by reconcile_stale; not an operator ruling",
            source="daemon",
        ),
        root,
    )


def reconcile_channel(
    pairs, *, root: Path, session_id: "str | None", cwd: Path,
    marker: str, subject: str, identities: "list[str]",
    question: "Callable[[str], str]", ask: "Callable[[str], str]",
) -> "tuple[str, str]":
    """Reconcile ONE durable ``[<marker>:<key>]`` operator question to the
    measured ``pairs``: same set is a duplicate, a changed set supersedes,
    an empty set closes. ``question``/``ask`` are callables taking the
    dedupe ``key``. Returns ``(outcome, question_id)`` with outcome in
    ``none | duplicate | asked | closed``."""
    key = dedupe_key(identities)

    if not pairs:
        open_qs = _open_questions(root, marker)
        for q in open_qs:
            _close_question(q.id, f"no {subject} rows remain at reconcile time", root)
        return ("closed", open_qs[0].id) if open_qs else ("none", "")

    existing = already_asked(root, key, marker=marker)
    if existing:
        # A previous run that died mid-close leaves superseded asks open.
        for q in _open_questions(root, marker):
            if q.id != existing:
                _close_question(
                    q.id, f"{subject} set changed; superseded by {existing}", root
                )
        return ("duplicate", existing)

    import secrets

    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    qid = f"q-{secrets.token_hex(4)}"
    # Append BEFORE closing superseded asks: a failed close must cost a
    # duplicate ask, never an empty channel.
    append_question_event(
        operator_question(
            question_id=qid,
            question=question(key),
            session_id=session_id,
            cwd=str(cwd),
            ask=ask(key),
            source="daemon",
        ),
        root,
    )
    for q in _open_questions(root, marker):
        if q.id != qid:
            _close_question(q.id, f"{subject} set changed; superseded by {qid}", root)
    return ("asked", qid)


def reconcile_stale(stale_pairs, *, root: Path, session_id: "str | None",
                    cwd: Path) -> "tuple[str, str]":
    """The stale lane's question: rows past the wake ceiling, oldest age
    named. See :func:`reconcile_channel` for the fold's contract."""
    shown = [
        f"{v.name} [node {_row.node or 'unknown'}]: {v.basis}"
        for v, _row in stale_pairs
    ]
    oldest = oldest_h([v.basis or "" for v, _row in stale_pairs])
    age_clause = f", oldest {oldest}h" if oldest is not None else ""
    return reconcile_channel(
        stale_pairs, root=root, session_id=session_id, cwd=cwd,
        marker=STALE_MARKER, subject="stale",
        identities=[f"stale:{v.row_id}" for v, _row in stale_pairs],
        question=lambda key: (
            f"[{STALE_MARKER}:{key}] The fleet watchdog holds "
            f"{len(stale_pairs)} stale row(s) no lane will act on{age_clause}. "
            "Nothing in the sweep clears these; each needs a human to reap "
            "it or resume it. Rows: " + "; ".join(shown)
        ),
        ask=lambda _key: (
            f"triage {len(stale_pairs)} stale watchdog row(s){age_clause}: "
            "fno agents watchdog --only stale"
        ),
    )


def run(*, json_out: bool) -> None:
    """The hidden stale-escalate verb's whole body, beside the fold it drives."""
    import json

    from fno.agents import watchdog as wd
    from fno.carveout.core import resolve_carveout_root, resolve_session_id

    payload, rows = wd.run_sweep()
    if payload.get("refused"):
        outcome, qid, stale_count, oldest = "refused", "", 0, 0
    else:
        stale_pairs = [
            (wd.Verdict(**data), row)
            for data, row in zip(payload["verdicts"], rows)
            if data["verdict"] == wd.STALE
        ]
        try:
            from fno.paths import resolve_repo_root

            session_id = resolve_session_id(resolve_repo_root())
        except Exception:  # noqa: BLE001 - an unbound ask still records
            session_id = None
        outcome, qid = reconcile_stale(
            stale_pairs,
            root=resolve_carveout_root(),
            session_id=session_id,
            cwd=Path.cwd(),
        )
        stale_count = len(stale_pairs)
        oldest = oldest_h([v.basis or "" for v, _row in stale_pairs]) or 0

    summary = f"Summary: {stale_count} stale, outcome {outcome}, oldest {oldest}h"
    if json_out:
        print(json.dumps({
            "outcome": outcome,
            "question_id": qid,
            "stale_count": stale_count,
            "oldest_h": oldest,
            "summary": summary,
        }), flush=True)
    else:
        print(summary, flush=True)
