"""fno pr CLI - in-package gh/git PR operations (ab-d4c98550).

Verbs:
    merge  - merge a PR with the fno-canonical guards (-> _merge.py)
    verify - audit an external PR gate, merged|reviews (-> _verify.py)
    rebase - two-phase rebase with conflict delegation (-> _rebase.py)
    logs   - tail the failing CI job, spool the rest (-> _logs.py)

The four ``scripts/lib/pr-*.sh`` were ported to in-package Python shelling to
gh/git, so these verbs run from a bare ``pip install fno`` with no repo-root
dependency. Each module preserves the bash exit-code / output contract.
"""
from __future__ import annotations

import enum
import os
from typing import Optional

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
        "Merge a PR via gh CLI with the fno-canonical guards (<pr_number>). "
        "Emits a JSON line {pr, outcome, reason, strategy}; exit 0 merged|queued, "
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
    "logs",
    help=(
        "Why did CI fail: spool the failing job's log to .fno/last-ci.log and "
        "print its last 40 lines. Omit <pr> to read the current branch. "
        "--job picks among several failures, --lines resizes the tail, --full "
        "dumps the whole log. Exit 0 green (nothing fetched), 1 red, 2 pending, "
        "3 no checks, 4 fetch error, 127 gh-missing."
    ),
)
def logs(
    pr_number: Optional[int] = typer.Argument(
        None, help="GitHub PR number; omitted -> the PR for the current branch."
    ),
    job: Optional[str] = typer.Option(
        None, "--job", help="Failing check to tail (exact name, else substring)."
    ),
    lines: int = typer.Option(40, "--lines", help="Tail length."),
    full: bool = typer.Option(False, "--full", help="Print the whole log, not a tail."),
) -> None:
    from fno.pr import _logs
    from fno.pr._proc import ToolMissing

    try:
        rc = _logs.run_logs(
            str(pr_number) if pr_number is not None else None,
            job=job,
            lines=lines,
            full=full,
        )
    except ToolMissing as exc:
        typer.echo(f"fno pr logs: {exc.tool} not found on PATH", err=True)
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
    "sync-canonical",
    help=(
        "Post-merge canonical-checkout sync (x-47be). Runs "
        "config.post_merge.sync_command in the CANONICAL checkout after a PR "
        "merges (opt-in; unset command = no-op). Exactly-once per merge SHA via "
        "a marker + single-flight lock; fail-open. Exit 0 no-op/skipped/synced, "
        "non-zero only on a failed sync_command (marker withheld -> retries)."
    ),
)
def sync_canonical(
    pr_number: int = typer.Option(..., "--pr-number", help="GitHub PR number of the merged PR"),
) -> None:
    from fno.pr import _sync_canonical

    rc = _sync_canonical.run_sync_canonical(pr_number)
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


@pr_app.command(
    "ritual",
    hidden=True,
    help=(
        "Mechanical core of the post-merge ritual (x-bbde). Runs the CLI-only "
        "steps as one idempotent sequence, printing a per-leg receipt line "
        "(step=<name> status=<ok|skipped|failed> detail=...). Non-zero if any "
        "leg failed; no leg is swallowed. The judgment residue (deferral triage "
        "+ parking-lot prose) is done inline by an attended caller, or spawned "
        "as one headless one-shot under --autonomous (never bg). Hidden: the "
        "`fno pr merged` skill is the attended front door."
    ),
)
def ritual(
    pr_number: Optional[int] = typer.Argument(
        None, help="Merged PR number; omitted -> most recently merged PR for this repo."
    ),
    autonomous: bool = typer.Option(
        False, "--autonomous",
        help="No operator present: spawn the judgment leg as one headless one-shot "
             "when its inputs are non-empty. Mirrors the POST_MERGE_NONINTERACTIVE=1 env.",
    ),
) -> None:
    from fno.pr import _ritual

    if os.environ.get("POST_MERGE_NONINTERACTIVE", "") == "1":
        autonomous = True
    rc = _ritual.run_ritual(pr_number, autonomous)
    raise typer.Exit(code=rc)
