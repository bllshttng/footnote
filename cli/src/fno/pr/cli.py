"""fno pr CLI - in-package gh/git PR operations (ab-d4c98550).

Verbs:
    merge  - merge a PR with the fno-canonical guards (-> _merge.py)
    verify - audit an external PR gate, merged|reviews (-> _verify.py)
    rebase - two-phase rebase with conflict delegation (-> _rebase.py)

The four ``scripts/lib/pr-*.sh`` were ported to in-package Python shelling to
gh/git, so these verbs run from a bare ``pip install fno`` with no repo-root
dependency. Each module preserves the bash exit-code / output contract.
"""
from __future__ import annotations

import enum

import typer


pr_app = typer.Typer(
    name="pr",
    help="PR utilities (merge / verify / rebase via gh + git)",
    no_args_is_help=True,
    add_completion=False,
)


class VerifyKind(str, enum.Enum):
    merged = "merged"
    reviews = "reviews"


@pr_app.command(
    "merge",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Merge a PR via gh CLI with the fno-canonical guards "
        "(--invoker=<target|megawalk> <pr_number>). Emits a JSON line "
        "{pr, outcome, reason, strategy, invoker}; exit 0 merged|queued, "
        "1 failed, 2 skipped, 127 gh-missing."
    ),
)
def merge(ctx: typer.Context) -> None:
    from fno.pr import _merge

    rc = _merge.run_merge(list(ctx.args))
    raise typer.Exit(code=rc)


@pr_app.command(
    "verify",
    help=(
        "Verify an external PR gate. --kind merged audits GitHub merge state "
        "(with a single bounded remediation); --kind reviews flips the "
        "external-review gate when a reviewer has no qualifying reply. "
        "Exit 0 clean/degrade, 1 blocked/flipped, 2 substrate failure."
    ),
)
def verify(
    kind: VerifyKind = typer.Option(..., help="Gate to verify: merged | reviews"),
    pr_number: int = typer.Option(..., "--pr-number", help="GitHub PR number"),
    state_file: str = typer.Option(..., "--state-file", help="Path to target-state.md"),
) -> None:
    from fno.pr import _verify

    if kind is VerifyKind.merged:
        rc = _verify.run_verify_merged(str(pr_number), state_file)
    else:
        rc = _verify.run_verify_reviews(str(pr_number), state_file)
    raise typer.Exit(code=rc)


@pr_app.command(
    "status",
    help=(
        "One authoritative CI verdict for a PR from statusCheckRollup. Prints a "
        "JSON line {pr, verdict, settled, green, checks}; exit 0 green, 1 red, "
        "2 pending, 3 unknown (no checks), 4 fetch error, 127 gh-missing. "
        "In-progress checks read as pending, never red."
    ),
)
def status(pr_number: int = typer.Argument(..., help="GitHub PR number")) -> None:
    from fno.pr import _status
    from fno.pr._proc import ToolMissing

    try:
        rc = _status.run_status(str(pr_number))
    except ToolMissing as exc:
        typer.echo(f"fno pr status: {exc.tool} not found on PATH", err=True)
        rc = 127
    raise typer.Exit(code=rc)


@pr_app.command(
    "base-check",
    help=(
        "Refuse a PR whose branch base is > 24h of main history behind "
        "origin/main (phantom-deletion guard). Exit 0 fresh|bypass|fail-open, "
        "3 stale (points at `fno pr rebase`), 4 unrelated histories. Bypass "
        "with FNO_PR_BASE_OK=stale-acknowledged (emits gate_escape)."
    ),
)
def base_check(
    base: str = typer.Option(
        "origin/main", "--base", help="Base ref to compare the branch against"
    ),
) -> None:
    from fno.pr import _preflight

    rc = _preflight.run_base_check(base=base)
    raise typer.Exit(code=rc)


@pr_app.command(
    "rebase",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Rebase the current branch via the conflict-delegation protocol "
        "(--base=<branch>, --continue). Two-phase: exit 42 means the caller "
        "must invoke the conflict-resolver agent, then call back --continue."
    ),
)
def rebase(ctx: typer.Context) -> None:
    from fno.pr import _rebase

    rc = _rebase.run_rebase(list(ctx.args))
    raise typer.Exit(code=rc)
