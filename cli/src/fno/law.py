"""One-step law recording: `fno inbox law set`.

The operator types one ruling and it records. There is no staged proposal, no
content hash, no one-shot approval receipt, and no resume path (ruling
d-e1eec854). The whole ceremony was deleted because it charged for a property it did not buy:
it refused the honest headless path at the last step while an attended chat
approved the same enactment without reading the hash.

WHAT THAT TRADE COSTS, measured rather than assumed, and stated at its real
WIDTH. `require_marked_caller` answers `chat_attested` off
`resolve_self_identity`, which walks process ancestry. So the door is not
"a mail-injected slash command". It is ANY process descended from a harness
session, including an agent's own Bash call with no user-shaped text anywhere.
The narrower mail shape is merely the one that is impossible to detect: across
every transcript in this machine's claude project directory, 2173 user turns
carrying an `<fno_mail>` envelope were recorded with `promptSource: "typed"`
and 2439 with `origin: {"kind": "human"}`, and `fno agents mail send --raw`
strips the envelope that is the one remaining marker.

What survives is the honest attribution: a chat recording lands as
`chat_attested`, never as `operator`, so a reader can always tell it from a
person at a terminal. Note the asymmetry that buys: a session can mint a law
row and cannot retract one, because `retract_decision` requires `operator`.
"""

from __future__ import annotations

import re

import typer

DECISION_ID_RE = re.compile(r"^d-[0-9a-f]{8}$")
COORDINATION_MARKERS = (
    "this pr",
    "this node",
    "this target",
    "temporary",
    "until merge",
    "for this change",
)


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

    This is the classification the retired `prepare` step used to own. It stays
    because it is the one part of the ceremony that read the STATEMENT rather
    than the session: a coordination note recorded as law is a lie a later
    reader cannot detect, and no amount of approval ritual fixes it.
    """
    if not subject.strip() or not decision.strip():
        raise LawValidationError("subject and decision are required")
    if not (rationale or "").strip():
        raise LawValidationError("rationale is required for durable law")
    lowered = decision.casefold()
    if any(marker in lowered for marker in COORDINATION_MARKERS):
        raise LawValidationError("the statement is coordination, not durable law")
    if supersedes and not DECISION_ID_RE.fullmatch(supersedes):
        raise LawValidationError("supersedes must be a decision id")


law_app = typer.Typer(help="Record operator law in one call.")


@law_app.command("set")
def record_command(
    subject: str = typer.Argument(..., help="Subject governed by the law."),
    decision: str = typer.Argument(..., help="Operator workaround or policy."),
    rationale: str | None = typer.Option(None, "--rationale"),
    option: list[str] = typer.Option([], "--option"),
    supersedes: str | None = typer.Option(None, "--supersedes"),
    graduation: str | None = typer.Option(None, "--graduation"),
    graduation_ref: str | None = typer.Option(None, "--graduation-ref"),
) -> None:
    """Record law in one call, from a chat or from a terminal."""
    from fno.decide import (
        IndexWriteError,
        RefusedAuthorityError,
        UnattributedAuthorityError,
        record_decision,
        require_marked_caller,
    )
    from fno.decide.graduation import InvalidGraduationError, graduation_or_guidance

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
        result = record_decision(
            subject=subject,
            decision=decision,
            rationale=rationale,
            options=list(option) or None,
            supersedes=supersedes,
            authority_source=authority,
            graduation=graduation_data,
        )
    except (InvalidGraduationError, ValueError) as exc:
        # ValueError is `record_decision` refusing a --supersedes that names no
        # recoverable decision. It must land on 3 with the rest: exit 1 is the
        # code reserved for "recorded, index write failed, do NOT re-run", so
        # letting it escape told a caller the opposite of what happened.
        typer.echo(f"fno law: refused: {exc}. Nothing was recorded.", err=True)
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
