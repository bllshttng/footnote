"""One-step law recording: `fno inbox law set`.

One ruling, recorded: no staged proposal, no resume path (ruling
d-e1eec854). The caller-resolver and the measured narrative:
docs/architecture/decision-record.md.
"""

from __future__ import annotations

from pathlib import Path

import typer

from fno.decide import READ_HELP


class LawValidationError(RuntimeError):
    """The statement is not a durable law statement."""


def validate_durable_law(
    *,
    subject: str,
    decision: str,
    rationale: str | None,
    supersedes: str | None = None,
) -> None:
    """Refuse a statement that is not durable law. Raises, or returns None.

    A coordination note recorded as law is a lie a later reader cannot detect.
    The statement rules and the node-id subject refusal live in the
    `law-match` crate verb (mode validate); this wrapper is the fail-closed
    transport, and an unavailable validator is a refusal, never a pass.
    """
    from fno.rust_binary import verb_call

    try:
        answer = verb_call(
            "law-match",
            {
                "mode": "validate",
                "subject": subject,
                "decision": decision,
                "rationale": rationale,
                "supersedes": supersedes,
            },
        )
    except Exception as exc:  # noqa: BLE001 - fail closed
        raise LawValidationError(f"law validation is unavailable ({exc})") from exc
    refusal = answer.get("refusal")
    if refusal:
        raise LawValidationError(refusal)


law_app = typer.Typer(help="Record operator law in one call.")


def _sweep_open_questions(subject: str, decision: str, decision_id: str) -> None:
    """Best-effort rule-time join : name the open questions the new
    law may answer. The matcher runs in the crate (`law-match`); the
    open-question fold stays in `read_open_questions`. A candidate is only a
    surface, never a verdict - three per-PR budget grants point the other way
    from the general law - so each line carries the mail command for the
    recording session to judge and send. It never raises and never touches
    stdout or the exit code: the law is already recorded here, and exit 1 is
    reserved for a failed index write.
    """
    try:
        from fno.outstanding.core import read_open_questions
        from fno.rust_binary import verb_call

        questions = read_open_questions(Path.cwd(), liveness_budget_seconds=0)
        answer = verb_call(
            "law-match",
            {
                "mode": "law",
                "law": {
                    "decision_id": decision_id,
                    "subject": subject,
                    "decision": decision,
                },
                "questions": [q.as_dict() for q in questions],
            },
        )
        for line in answer.get("lines") or []:
            typer.echo(line, err=True)
    except Exception as exc:  # noqa: BLE001 - the law is already recorded
        typer.echo(f"law: open-question sweep failed ({exc}); the law is recorded", err=True)


@law_app.callback()
def _law_callback() -> None:
    """Hold `set` as a named subcommand on BOTH mounts.

    Not dead code, and the round-1 review's read that it was rested on the
    wrong mount. `inbox_app.add_typer(law_app, name="law")` builds a group
    either way, so `fno inbox law set` survives without this. The deprecated
    root `fno law` shim goes through the lazy-loader table instead, and there
    a single-command app collapses its one command into the group.

    Measured, not reasoned about: deleting this callback makes the verb ratchet
    report `law` added and `law set` removed against
    `scripts/ci/verb-baseline.txt`. The callback is what keeps the two mounts
    spelling the verb the same way.
    """



@law_app.command("set")
def record_command(
    subject: str = typer.Argument(..., help="Subject governed by the law."),
    decision: str | None = typer.Argument(None, help="Operator workaround or policy."),
    decision_file: Path | None = typer.Option(
        None, "--decision-file", help="Read the decision from a file ('-' = stdin)."
    ),
    rationale: str | None = typer.Option(None, "--rationale"),
    option: list[str] = typer.Option([], "--option"),
    supersedes: str | None = typer.Option(None, "--supersedes"),
    graduation: str | None = typer.Option(None, "--graduation"),
    graduation_ref: str | None = typer.Option(None, "--graduation-ref"),
    read: list[str] = typer.Option([], "--read", help=READ_HELP),
) -> None:
    """Record law in one call, from a chat or from a terminal."""
    from fno.decide import (
        IndexWriteError,
        RefusedAuthorityError,
        UnattributedAuthorityError,
        WaiverAuthorityRefusedError,
        record_decision,
        require_marked_caller,
    )
    from fno.decide.graduation import InvalidGraduationError, graduation_or_guidance
    from fno.rust_binary import VerbUnavailable, verb_call
    from fno.text_or_file import read_text_arg

    decision = read_text_arg(decision, decision_file, what="the decision") or ""

    try:
        validate_durable_law(
            subject=subject,
            decision=decision,
            rationale=rationale,
            supersedes=supersedes,
        )
    except LawValidationError as exc:
        typer.echo(f"fno law: refused: {exc}. Nothing was recorded.", err=True)
        raise typer.Exit(3) from exc

    try:
        authority = require_marked_caller()
        graduation_data = graduation_or_guidance(graduation, graduation_ref)
        # The door fails closed: no project, no stamp, no row. The --global
        # widening lives on the crate verb; this surface stamps the current
        # project only.
        answer = verb_call("law-match", {"mode": "record-scope", "global": False})
        scope = answer.get("scope")
        if not scope:
            raise ValueError(str(answer.get("refusal") or "no project to stamp under"))
        result = record_decision(
            subject=subject,
            decision=decision,
            rationale=rationale,
            options=list(option) or None,
            supersedes=supersedes,
            authority_source=authority,
            graduation=graduation_data,
            reads=list(read) or None,
            scope=scope,
        )
    except (InvalidGraduationError, ValueError, VerbUnavailable) as exc:
        # ValueError is `record_decision` refusing a --supersedes that names no
        # recoverable decision; VerbUnavailable is the evidence gate refusing
        # to run at all. Both must land on 3 with the rest: exit 1 is the
        # code reserved for "recorded, index write failed, do NOT re-run", so
        # letting either escape told a caller the opposite of what happened.
        typer.echo(f"fno law: refused: {exc}. Nothing was recorded.", err=True)
        raise typer.Exit(3) from exc
    except WaiverAuthorityRefusedError as exc:
        typer.echo(f"fno law: refused: {exc}", err=True)
        raise typer.Exit(3) from exc
    except (RefusedAuthorityError, UnattributedAuthorityError) as exc:
        typer.echo(
            f"fno law: refused: {exc}. Append agent findings with "
            "`fno backlog note <node> <text>`.",
            err=True,
        )
        raise typer.Exit(3) from exc
    except IndexWriteError as exc:
        typer.echo(
            f"fno law: recorded {exc.decision_id} to the project journal, but "
            "the recall index write failed. Run `fno backlog decide-reindex`; "
            "do not re-run the law command.",
            err=True,
        )
        raise typer.Exit(1) from exc
    typer.echo(result["decision_id"])
    _sweep_open_questions(subject, decision, result["decision_id"])
