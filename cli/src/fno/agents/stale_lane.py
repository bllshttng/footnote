"""The stale-row question lane: reconcile one ``[watchdog-stale:*]`` operator
question to the set the sweep actually measured. A row past the wake ceiling
is the needs-human bucket - no action lane may take it, so the only honest
surface is a human's. The generic ``reconcile_channel`` below serves every
report-only question lane (the friction lane rides it too). Deliberately NOT
the report path: the AC9 census guards ``stale_escalate.py`` against
session-bookkeeping vocabulary (PR 1227 measured the stale ask as noise).
"""
from __future__ import annotations

import re
from collections.abc import Sequence
from pathlib import Path

from fno.agents.stale_escalate import already_asked, answered_question, dedupe_key, reset_answered

STALE_MARKER = "watchdog-stale"
#: The reaper-hold lane (x-e3cc): escalated holds ask on the same durable
#: question channel the stale lane uses.
HOLD_MARKER = "reap-hold"

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


def _close_question(qid: str, answer: str, root: Path, *, lane: str = "stale") -> None:
    """Close one ask AND record the decision the close made (the stop gate
    holds a closed-with-answer question with no ``operator_decision``
    record): a mechanical supersede, never an operator ruling. ``lane`` is
    the channel short name, so friction closes record friction provenance."""
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


def reconcile_channel(
    pairs, *, root: Path, session_id: "str | None", cwd: Path,
    marker: str, subject: str, identities: "list[str]",
    question, ask, blocks: "Sequence[str]" = (),
) -> "tuple[str, str]":
    """Reconcile ONE durable ``[<marker>:<key>]`` operator question to the
    measured ``pairs``: same set is a duplicate, a changed set supersedes,
    an empty set closes, and a set a human already answered stays answered.
    ``question``/``ask`` are callables taking the dedupe ``key``; outcome in
    ``none | duplicate | answered | asked | closed``. ``blocks`` carries the
    node ids the question is about, so the ask gate's pointer rule is met
    from the run's own facts."""
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
                _close_question(q.id, f"{subject} set changed; superseded by {existing}",
                                root, lane=subject)
        return ("duplicate", existing)
    answered = answered_question(root, key, marker=marker)
    if answered:
        for q in _open_questions(root, marker):
            if q.id != answered:
                _close_question(q.id, f"{subject} set changed; superseded by {answered}",
                                root, lane=subject)
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
            ask=ask(key),
            source="daemon",
            blocks=sorted(set(blocks)) or None,
        ),
        root,
    )
    for q in _open_questions(root, marker):
        if q.id != qid:
            _close_question(q.id, f"{subject} set changed; superseded by {qid}",
                            root, lane=subject)
    return ("asked", qid)


def reconcile_stale(stale_pairs, *, root: Path, session_id: "str | None",
                    cwd: Path) -> "tuple[str, str]":
    """The stale lane's question: rows past the wake ceiling, oldest age
    named (see :func:`reconcile_channel`). Rows live in ``blocks``."""
    oldest = oldest_h([v.basis or "" for v, _row in stale_pairs])
    age_clause = f", oldest {oldest}h" if oldest is not None else ""
    return reconcile_channel(
        stale_pairs, root=root, session_id=session_id, cwd=cwd,
        marker=STALE_MARKER, subject="stale",
        identities=[f"stale:{v.row_id}" for v, _row in stale_pairs],
        question=lambda key: (
            f"[{STALE_MARKER}:{key}] The watchdog holds "
            f"{len(stale_pairs)} stale row(s) no lane will act on{age_clause}."
        ),
        ask=lambda _key: (
            f"triage {len(stale_pairs)} stale watchdog row(s){age_clause}: "
            "fno agents watchdog --only stale"
        ),
        blocks=sorted({_row.node for _v, _row in stale_pairs if _row.node}),
    )


def reconcile_holds(holds, *, root: Path, session_id: "str | None",
                    cwd: Path) -> "tuple[str, str]":
    """The reap-hold lane's question: holds past agents.hold_escalate_after_s
    (see :func:`reconcile_channel`). Identities are ``hold:<id>:<reason>``,
    so a hold that changes reason asks again. The ask is the first row's
    release command."""
    if not holds:
        # An empty set closes: the channel decides, never the caller.
        return reconcile_channel(
            [], root=root, session_id=session_id, cwd=cwd,
            marker=HOLD_MARKER, subject="reap-hold",
            identities=[],
            question=lambda key: "",
            ask=lambda _key: "",
        )
    ask_cmd = f"fno agents reap --release {holds[0]['id']}"
    return reconcile_channel(
        holds, root=root, session_id=session_id, cwd=cwd,
        marker=HOLD_MARKER, subject="reap-hold",
        identities=[f"hold:{h['id']}:{h['reason']}" for h in holds],
        question=lambda key: (
            f"[{HOLD_MARKER}:{key}] The reaper holds "
            f"{len(holds)} row(s) past agents.hold_escalate_after_s."
        ),
        ask=lambda _key: ask_cmd,
    )


def escalated_holds():
    """The escalated holds from one reap dry-run read; ``None`` when the
    read fails, so the channel closes nothing on an unreadable instrument."""
    import json

    from fno.agents import retirement as retirement_mod

    try:
        summary = json.loads(retirement_mod._default_runner())
    except Exception:  # noqa: BLE001 - an unreadable instrument asks nothing
        return None
    return [h for h in summary.get("holds", []) if h.get("escalated")]


def run(*, json_out: bool) -> None:
    """The hidden stale-escalate verb's whole body, beside the fold it drives."""
    import json

    from fno.agents import watchdog as wd
    from fno.carveout.core import resolve_carveout_root, resolve_session_id

    root = resolve_carveout_root()
    try:
        from fno.paths import resolve_repo_root

        session_id = resolve_session_id(resolve_repo_root())
    except Exception:  # noqa: BLE001 - an unbound ask still records
        session_id = None
    cwd = Path.cwd()

    # The hold channel (x-e3cc) runs even when the watchdog sweep refused:
    # the reaper's clock is a different instrument, and its question must
    # not vanish behind the watchdog's own refusal.
    holds = escalated_holds()
    if holds is None:
        hold_count, hold_outcome, hold_qid = 0, "refused", ""
    else:
        hold_outcome, hold_qid = reconcile_holds(
            holds, root=root, session_id=session_id, cwd=cwd,
        )
        hold_count = len(holds)

    payload, rows = wd.run_sweep()
    if payload.get("refused"):
        outcome, qid, stale_count, oldest = "refused", "", 0, 0
    else:
        stale_pairs = [
            (wd.Verdict(**data), row)
            for data, row in zip(payload["verdicts"], rows)
            if data["verdict"] == wd.STALE
        ]
        outcome, qid = reconcile_stale(
            stale_pairs,
            root=root,
            session_id=session_id,
            cwd=cwd,
        )
        stale_count = len(stale_pairs)
        oldest = oldest_h([v.basis or "" for v, _row in stale_pairs]) or 0

    summary = (
        f"Summary: {stale_count} stale, outcome {outcome}, oldest {oldest}h; "
        f"{hold_count} escalated hold(s), outcome {hold_outcome}"
    )
    if json_out:
        print(json.dumps({
            "outcome": outcome,
            "question_id": qid,
            "stale_count": stale_count,
            "oldest_h": oldest,
            "hold_count": hold_count,
            "hold_outcome": hold_outcome,
            "hold_question_id": hold_qid,
            "summary": summary,
        }), flush=True)
    else:
        print(summary, flush=True)
