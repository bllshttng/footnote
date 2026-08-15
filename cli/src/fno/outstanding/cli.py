"""`fno outstanding` - read what is waiting on a human; ask and clear questions.

Machine-first, mirroring `fno carveout`: stdout carries the value (the report,
or a new question id), guidance and warnings go to stderr, and exit codes are
predictable (0 ok / 1 read or write failure).
"""
from __future__ import annotations

import json
import secrets
from pathlib import Path
from typing import List

import typer

from fno.outstanding.core import OutstandingError, collect, render

outstanding_app = typer.Typer(
    help=(
        "What is outstanding FOR YOU: unharvested carve-outs, open operator "
        "questions, and undecided fu- captures folded across every project. "
        "Read-only by default; `ask` records a question so it survives the "
        "next turn, `clear` closes it once answered."
    ),
)


def _storage_root() -> Path:
    """The canonical root that owns both stores (shared project state)."""
    from fno.carveout.core import resolve_carveout_root

    return resolve_carveout_root()


def _session_id() -> "str | None":
    from fno.carveout.core import resolve_session_id
    from fno.paths import resolve_repo_root

    try:
        return resolve_session_id(resolve_repo_root())
    except Exception:  # noqa: BLE001 - an unresolvable session is not an error here
        return None


@outstanding_app.callback(invoke_without_command=True)
def report(
    ctx: typer.Context,
    as_json: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit one JSON object carrying both legs instead of the human block.",
    ),
) -> None:
    """Report unharvested carve-outs and open operator questions."""
    if ctx.invoked_subcommand is not None:
        return

    root = _storage_root()
    try:
        outstanding = collect(root)
    except OutstandingError as exc:
        # A present-but-unreadable store is a FAILED read, never "nothing
        # outstanding": reporting an empty queue here would tell the operator
        # the pile is clear when it is merely unreadable.
        typer.echo(f"outstanding: failed to read: {exc}", err=True)
        raise typer.Exit(1)

    if as_json:
        typer.echo(json.dumps(outstanding.as_dict(), separators=(",", ":")))
        return

    block = render(outstanding, session_id=_session_id())
    if block:
        typer.echo(block, nl=False)


@outstanding_app.command("ask")
def ask(
    question: str = typer.Argument(
        ..., help="What you need the operator to decide or answer."
    ),
    node: str = typer.Option(
        None, "--node", help="Backlog node the question is about, when there is one."
    ),
) -> None:
    """Record a question for the operator so it survives the next turn.

    The capture is the point. `session_truth` classifies from the transcript
    tail, so an unrecorded question stops existing the moment another turn
    lands - and in a mail-driven mesh the very next turn is usually the agent
    answering some mail.
    """
    from fno.events import append_event, operator_question
    from fno.outstanding.core import events_path

    qid = f"q-{secrets.token_hex(4)}"
    session_id = _session_id()
    try:
        append_event(
            operator_question(
                question_id=qid,
                question=question,
                session_id=session_id,
                cwd=str(Path.cwd()),
                node=node,
            ),
            events_path=events_path(_storage_root()),
        )
    except Exception as exc:  # noqa: BLE001 - a failed capture is never a silent success
        typer.echo(f"outstanding: failed to record question: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(
        f"outstanding: recorded {qid}. Clear it once answered: "
        f'fno outstanding clear {qid} --answer "..."',
        err=True,
    )
    # stdout carries the value: the new question id.
    typer.echo(qid)


@outstanding_app.command("clear")
def clear(
    question_ids: List[str] = typer.Argument(
        ..., help="Question id(s) to close (e.g. q-ab12cd34)."
    ),
    answer: str = typer.Option(
        None, "--answer", help="The answer, recorded so the decision outlives the session."
    ),
) -> None:
    """Close one or more open questions. Idempotent."""
    from fno.events import append_event, operator_decision, operator_question_closed
    from fno.outstanding.core import events_path, read_open_questions

    root = _storage_root()
    try:
        open_by_id = {q.id: q for q in read_open_questions(root)}
    except OutstandingError as exc:
        typer.echo(f"outstanding: failed to read: {exc}", err=True)
        raise typer.Exit(1)

    # An id that is not open is a no-op, not a failure: mirroring
    # `carveout resolve`, a second clear must be safe to run.
    targets = [q for q in question_ids if q in open_by_id]
    skipped = [q for q in question_ids if q not in open_by_id]

    closed_by = _session_id()
    for qid in targets:
        try:
            append_event(
                operator_question_closed(
                    question_id=qid, answer=answer, closed_by=closed_by
                ),
                events_path=events_path(root),
            )
            # An answered question IS a decision, so the close path records it
            # as one (AC3). A close with no answer is a withdrawal and decides
            # nothing. Emitted alongside the closed event, never instead of it.
            if answer is not None:
                record = open_by_id[qid]
                append_event(
                    operator_decision(
                        decision_id=f"d-{secrets.token_hex(4)}",
                        decision=answer,
                        subject=record.node,
                        question_id=qid,
                        question=record.question,
                        asked_by=record.session_id,
                        asked_at=record.ts or None,
                        # The decider is the operator by definition on this
                        # path (closed_by records the typing session on the
                        # close event); decided_by must not depend on whether
                        # the closing session could be resolved.
                        decided_by="operator",
                        authority_source="operator",
                    ),
                    events_path=events_path(root),
                )
        except Exception as exc:  # noqa: BLE001
            typer.echo(f"outstanding: failed to close {qid}: {exc}", err=True)
            raise typer.Exit(1)

    if skipped:
        typer.echo(
            f"outstanding: not open, nothing to close: {', '.join(skipped)}", err=True
        )
    typer.echo(str(len(targets)))
