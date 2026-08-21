"""`fno inbox outstanding` - read what is waiting on a human; ask and clear questions.

Machine-first, mirroring `fno carveout`: stdout carries the value (the report,
or a new question id), guidance and warnings go to stderr, and exit codes are
predictable (0 ok / 1 read or write failure).
"""

from __future__ import annotations

import json
import os
import secrets
from pathlib import Path
from typing import List

import typer

from fno.king.lane import read_lane
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
    from fno.outstanding.core import _canonical_checkout_for

    env_root = os.environ.get("FNO_REPO_ROOT")
    if env_root:
        return Path(env_root).resolve()

    cwd = Path.cwd()
    marker = cwd / ".git"
    if marker.is_dir() or marker.is_file():
        canonical = _canonical_checkout_for(cwd)
        if canonical != cwd or marker.is_dir():
            return canonical

    from fno.paths import resolve_repo_root

    root = resolve_repo_root()
    canonical = _canonical_checkout_for(root)
    if canonical != root or (root / ".git").is_dir():
        return canonical

    from fno.carveout.core import resolve_carveout_root

    return resolve_carveout_root()


def _session_id() -> "str | None":
    from fno.paths import resolve_repo_root

    try:
        lines = (resolve_repo_root() / ".fno" / "target-state.md").read_text(
            encoding="utf-8"
        ).splitlines()
    except Exception:  # noqa: BLE001 - an unresolvable session is not an error here
        lines = []
    fields: dict[str, str] = {}
    if lines and lines[0].strip() == "---":
        closed = False
        for line in lines[1:]:
            stripped = line.strip()
            if stripped == "---":
                closed = True
                break
            for key in ("fno_id", "session_id"):
                prefix = f"{key}:"
                if stripped.startswith(prefix):
                    value = stripped[len(prefix) :].strip().strip("\"'")
                    if value and value != "null":
                        fields.setdefault(key, value)
        if closed:
            session_id = fields.get("fno_id") or fields.get("session_id")
            if session_id:
                return session_id
    env_session = os.environ.get("CLAUDECODE_SESSION_ID")
    return env_session.strip() if env_session and env_session.strip() else None


def _is_crowned() -> bool:
    """True only when this session's registry row carries a crown.

    ``FNO_AGENT_SELF`` (tier 1 of ``resolve_self``) answers with no
    session-id dependency, which is what lets a spawned king resolve this at
    session start before the harness has written anything else. Any
    exception degrades to "not crowned" and never fails the command:
    `fno outstanding` runs on the session-start path under a bound, and
    degrading this way costs one missing action line, not a missing lane.
    """
    try:
        from fno.agents.registry import RegistryVersionError, load_registry
        from fno.agents.whoami import resolve_self
        from fno.harness_identity import resolve_harness_identity

        registry: list = []
        try:
            registry = load_registry()
        except RegistryVersionError:
            pass
        ident = resolve_harness_identity()
        result = resolve_self(
            env=os.environ,
            registry=registry,
            session_uuid=ident.session_id,
            harness=ident.harness,
        )
    except Exception:  # noqa: BLE001 - an unresolved crown is not an error here
        return False
    return result.crown is not None


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
        outstanding = collect(root, lane=read_lane())
    except OutstandingError as exc:
        # A present-but-unreadable store is a FAILED read, never "nothing
        # outstanding": reporting an empty queue here would tell the operator
        # the pile is clear when it is merely unreadable.
        typer.echo(f"outstanding: failed to read: {exc}", err=True)
        raise typer.Exit(1)

    if as_json:
        typer.echo(json.dumps(outstanding.as_dict(), separators=(",", ":")))
        return

    block = render(outstanding, session_id=_session_id(), crowned=_is_crowned())
    if block:
        typer.echo(block, nl=False)


@outstanding_app.command("ask")
def ask(
    question: str = typer.Argument(..., help="What you need the operator to decide or answer."),
    ask: str = typer.Option(None, "--ask", help="One action that closes the question."),
    option: List[str] = typer.Option(
        [], "--option", help="A choice the operator can make; repeatable."
    ),
    blocks: List[str] = typer.Option(
        [], "--blocks", help="A backlog node blocked by the question; repeatable."
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
    from fno.events import operator_question
    from fno.harness_identity import canonical_handle, resolve_harness_identity
    from fno.outstanding.core import QuestionIndexWriteError, append_question_event

    qid = f"q-{secrets.token_hex(4)}"
    session_id = _session_id()
    ident = resolve_harness_identity()
    asker = canonical_handle(ident.session_id) if ident.session_id and ident.harness else None
    try:
        event = operator_question(
            question_id=qid,
            question=question,
            session_id=session_id,
            cwd=str(Path.cwd()),
            node=node,
            asker=asker,
            ask=ask,
            options=option or None,
            blocks=blocks or None,
        )
        append_question_event(event, _storage_root())
    except QuestionIndexWriteError as exc:
        typer.echo(
            f"outstanding: recorded {exc.question_id} in the project journal, "
            f"but the recall index write failed: {exc}. Run "
            "`fno inbox outstanding reindex`; do not retry ask, which would mint a "
            "second id for the same question.",
            err=True,
        )
        raise typer.Exit(1)
    except Exception as exc:  # noqa: BLE001 - a failed capture is never a silent success
        typer.echo(f"outstanding: failed to record question: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(
        f"outstanding: recorded {qid}. Clear it once answered: "
        f'fno inbox outstanding clear {qid} --answer "..."',
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
    authority: str = typer.Option(
        None,
        "--authority",
        help="How the answerer was entitled to answer: 'operator', 'crown', "
        "'agent', or 'beastmode'. Omit to claim none.",
    ),
) -> None:
    """Close one or more open questions. Idempotent."""
    from fno.decide import (
        AUTHORITY_SOURCES,
        IndexWriteError,
        RefusedAuthorityError,
        UnattributedAuthorityError,
    )
    from fno.events import operator_question_closed
    from fno.outstanding.core import (
        QuestionIndexWriteError,
        append_question_event,
        read_open_questions,
    )

    # Answering a question the operator was formally asked is the canonical
    # law-producing act in this system. Without this flag `clear` had nothing
    # to say once law stopped being defaulted, so the one path that most
    # deserves the operator lane could never reach it.
    if authority is not None and authority not in AUTHORITY_SOURCES:
        typer.echo(
            f"outstanding: --authority '{authority}' is not one of "
            f"{', '.join(AUTHORITY_SOURCES)}. Nothing was closed.",
            err=True,
        )
        raise typer.Exit(2)

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
            # An answered question IS a decision, so the close path records it
            # as one. A close with no answer is a withdrawal and decides
            # nothing. Record it before closing, so a failure to record leaves
            # the question open and therefore retryable. That no longer covers
            # a failed graph PROJECTION: the machine-wide index carries recall
            # now, so the answer is already findable and a retry would record
            # it twice.
            # Routed through record_decision so the decision gets both halves
            # a `fno backlog decide` record has: the event AND the graph projection
            # onto the subject node, which is what `fno backlog decisions` reads.
            # decided_by is left unset on purpose, so record_decision stamps
            # whoever actually ran the verb. Naming the operator here would
            # assert it, and an agent clearing a question on their behalf would
            # then be indistinguishable from the operator answering it.
            recorded: "str | None" = None
            if answer is not None:
                from fno.decide import record_decision

                record = open_by_id[qid]
                try:
                    recorded = record_decision(
                        decision=answer,
                        subject=record.node,
                        question_id=qid,
                        question=record.question,
                        asked_by=record.asker or record.session_id,
                        asked_at=record.ts or None,
                        authority_source=authority,
                        events_root=root,
                    )["decision_id"]
                except (RefusedAuthorityError, UnattributedAuthorityError) as exc:
                    # Refuse the CLOSE too, never just the decision. Closing a
                    # question whose answer was refused would retire it with
                    # nothing on record, which is the worse of the two.
                    #
                    # Say NOTHING was closed, not that this id stayed open.
                    # _resolve_decider is a pure function of the ambient
                    # identity and the one --authority value, both invariant
                    # across this loop, so the refusal fires on the first id or
                    # never. Naming one id would be the partial statement this
                    # verb's whole family of messages exists to stop.
                    typer.echo(
                        f"outstanding: refused: {exc}. Nothing was closed; "
                        f"all {len(targets)} question(s) stay open.",
                        err=True,
                    )
                    raise typer.Exit(3)
            try:
                event = operator_question_closed(
                    question_id=qid, answer=answer, closed_by=closed_by
                )
                append_question_event(event, root)
            except QuestionIndexWriteError:
                raise
            except Exception as exc:  # noqa: BLE001
                # The close failed AFTER the decision landed (a contended
                # journal lock, ENOSPC). The generic handler below would say
                # "failed to close", the operator would re-run with --answer,
                # and one answer would be recorded twice under two ids with no
                # supersedes link. Name the id and the safe way out instead.
                if recorded is None:
                    raise
                typer.echo(
                    f"outstanding: the answer to {qid} is recorded as {recorded}, "
                    f"but closing the question failed: {exc}. It stays open. "
                    f"Close it with no --answer; clearing with --answer again "
                    f"records the same ruling a second time.",
                    err=True,
                )
                raise typer.Exit(1)
        except IndexWriteError as exc:
            # The other producer of operator_decision, and it must give the
            # same guidance: the durable event landed, so the documented
            # "just run it again" would record one answer twice under two ids.
            # A guard on one of two producer paths is decorative.
            typer.echo(
                f"outstanding: recorded the answer to {qid} as {exc.decision_id}, "
                f"but the recall index write failed: {exc}. The question stays "
                f"open. Run `fno backlog decide-reindex` to recover the answer, then "
                f"close it with no --answer; clearing with --answer again "
                f"records the same ruling a second time.",
                err=True,
            )
            raise typer.Exit(1)
        except QuestionIndexWriteError as exc:
            typer.echo(
                f"outstanding: recorded the close for {exc.question_id} in the "
                f"project journal, but the recall index write failed: {exc}. "
                "Run `fno inbox outstanding reindex`; do not retry clear blindly.",
                err=True,
            )
            raise typer.Exit(1)
        except typer.Exit:
            # typer.Exit derives from RuntimeError, so the generic handler
            # below would swallow the precise message just raised and replace
            # it with "failed to close".
            raise
        except Exception as exc:  # noqa: BLE001
            typer.echo(f"outstanding: failed to close {qid}: {exc}", err=True)
            raise typer.Exit(1)

    if skipped:
        typer.echo(
            f"outstanding: not open, nothing to close: {', '.join(skipped)}", err=True
        )
    typer.echo(str(len(targets)))


@outstanding_app.command("reindex")
def reindex() -> None:
    """Backfill the machine-wide question index from every project journal."""
    from fno.outstanding.core import reindex_questions

    try:
        result = reindex_questions(_storage_root())
    except OutstandingError as exc:
        typer.echo(f"outstanding: failed to read: {exc}", err=True)
        raise typer.Exit(1)
    except Exception as exc:  # noqa: BLE001 - a partial backfill is not success
        typer.echo(f"outstanding: failed to reindex questions: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(
        f"{result['added']} event(s) added; {result['already']} already indexed; "
        f"{result['total']} total"
    )
