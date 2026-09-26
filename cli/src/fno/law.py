"""One-step law recording: `fno inbox law set`.

One ruling, recorded: no staged proposal, no resume path (ruling
d-e1eec854). The caller-resolver and the measured narrative:
docs/architecture/decision-record.md.
"""

from __future__ import annotations

import subprocess

import typer


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



@law_app.command(
    "set",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def record_command(ctx: typer.Context) -> None:
    """Record law in one call, from a chat or from a terminal.

    The door lives in the law-match record mode (`fno-agents law-match
    record`): this command forwards its arguments verbatim, so --global and
    --paths are native flags of the Rust door, and mirrors its exit code
    (0 recorded, 1 recorded-but-index-failed, 3 refused).
    """
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno law: refused: the law-match binary is unavailable. "
            "Nothing was recorded.",
            err=True,
        )
        raise typer.Exit(3)
    completed = subprocess.run([str(binary), "law-match", "record", *ctx.args])
    raise typer.Exit(completed.returncode)
