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
import json
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
        "Emits a JSON line {pr, outcome, reason, strategy[, cleanup]}; exit 0 merged, "
        "1 failed, 2 skipped|held, 127 gh-missing."
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
        "In-progress checks read as pending, never red. settled is true only "
        "when every latest run carries a real conclusion, so a cancelled run "
        "reads red AND unsettled."
    ),
)
def status(pr_number: int = typer.Argument(..., help="GitHub PR number")) -> None:
    from fno.pr import _status

    # main() routes through the coalescing cache: the watcher
    # recipe polls this verb every 60s per session, and N sessions polling one
    # PR must collapse to one network read per TTL or they trip the REST
    # secondary limit (which counts request rate, not budget).
    rc = _status.main([str(pr_number)])
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
    "base-lineage-check",
    help=(
        "Refuse a merge into a base branch that no longer leads to the default "
        "branch (<pr_number>). Fires when a MERGED PR already carried that base, "
        "or when the base is already an ancestor of the default branch. Exit 0 "
        "ok|bypassed, 3 stale (retarget the PR), 4 unknown (a probe failed). "
        "Bypass with FNO_PR_BASE_LINEAGE_OK=stale-acknowledged (emits gate_escape)."
    ),
)
def base_lineage_check(
    pr_number: int = typer.Argument(..., help="GitHub PR number"),
) -> None:
    from fno.pr import _base_lineage
    from fno.pr._proc import ToolMissing

    try:
        rc = _base_lineage.run_base_lineage_check(pr_number)
    except ToolMissing as exc:
        typer.echo(f"fno pr base-lineage-check: {exc.tool} not found on PATH", err=True)
        rc = 127
    raise typer.Exit(code=rc)


@pr_app.command(
    "coverage-check",
    hidden=True,
    help=(
        "The merge guard's coverage predicate, for callers that cannot import "
        "it (the stdlib-only git-protection hook shells this verb on a bare "
        "gh pr merge). Exit 0 covered, 3 uncovered (the guard's refusal on "
        "stderr), 4 unanswered (a named instrument failure)."
    ),
)
def coverage_check(
    pr_number: int = typer.Argument(..., help="GitHub PR number"),
    recompute: bool = typer.Option(
        False,
        "--recompute",
        help="Fire the Rust producer once when no row describes this head.",
    ),
) -> None:
    from fno.pr import _coverage_gate

    raise typer.Exit(code=_coverage_gate.run_coverage_check(pr_number, recompute=recompute))


@pr_app.command(
    "evidence-check",
    help=(
        "Require a newest exact-HEAD full/passed verification receipt across "
        "project, global, and known delivery-root event journals. "
        "--allow-rebase-equivalent also accepts a full/passed receipt for an "
        "earlier commit whose verbatim patch ids match HEAD (a rebase with no "
        "code change; never a rescue of a failed or pending receipt for HEAD "
        "itself); review entry opts in, the attestation reuse path never does."
    ),
)
def evidence_check(
    allow_rebase_equivalent: bool = typer.Option(
        False,
        "--allow-rebase-equivalent",
        help="Accept a receipt for an earlier commit whose patches match HEAD.",
    ),
) -> None:
    from fno.pr import _preflight

    raise typer.Exit(
        code=_preflight.run_evidence_check(allow_equivalent=allow_rebase_equivalent)
    )


@pr_app.command("evidence-required", hidden=True)
def evidence_required(
    base: str = typer.Option("origin/main", "--base"),
) -> None:
    """Expose the one local-verification policy to shell-based ship paths."""
    from fno.pr import _preflight

    required, reason = _preflight.local_verification_required(
        cwd=os.getcwd(), base_ref=base
    )
    typer.echo(json.dumps({"required": required, "reason": reason}, separators=(",", ":")))


@pr_app.command("next-receipt-generation", hidden=True)
def next_receipt_generation(
    candidate_sha: str = typer.Option(..., "--candidate-sha"),
) -> None:
    """Derive the next receipt generation from every discovered journal."""
    from fno.pr import _preflight

    try:
        generation = _preflight.next_verification_generation(
            cwd=os.getcwd(), candidate_sha=candidate_sha
        )
    except ValueError as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(code=1)
    typer.echo(str(generation))


@pr_app.command("global-receipt-events-path", hidden=True)
def global_receipt_events_path() -> None:
    """Print the durable cross-checkout verification receipt journal."""
    from fno.paths import global_events_json

    typer.echo(str(global_events_json()))


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
    "publish-review",
    hidden=True,
    help=(
        "Post a review verdict to GitHub as config.review.bot_identity "
        "(--pr N required; --verdict defaults to the newest head-pinned "
        "review_attestation for HEAD; --dry-run resolves and refuse-checks "
        "but makes no POST). Prints one bot-review: receipt line; exit 0 "
        "posted, 1 skipped|refused|failed, 2 no attestation to default from."
    ),
)
def publish_review_cmd(
    pr_number: int = typer.Option(..., "--pr", help="GitHub PR number"),
    verdict: Optional[str] = typer.Option(
        None, "--verdict", help="pass | fail; default: newest head-pinned attestation for HEAD."
    ),
    dry_run: bool = typer.Option(
        False, "--dry-run", help="Resolve and refuse-check, but make no POST."
    ),
) -> None:
    from fno.pr import _publish_review as pub

    cwd = os.getcwd()
    attestation = pub.newest_head_attestation(cwd)
    if verdict is None:
        if not attestation:
            typer.echo(
                "publish-review: no head-pinned attestation for HEAD; "
                "pass --verdict explicitly",
                err=True,
            )
            raise typer.Exit(code=2)
        verdict = str(attestation.get("verdict") or "")
    if verdict not in ("pass", "fail"):
        typer.echo(f"publish-review: --verdict must be pass|fail, got {verdict!r}", err=True)
        raise typer.Exit(code=2)
    reviewer = str((attestation or {}).get("reviewer") or "manual")
    head_sha = str((attestation or {}).get("head_sha") or pub._git_head(cwd) or "")
    result = pub.publish_review(
        pr_number=pr_number,
        head_sha=head_sha,
        verdict=verdict,
        reviewer=reviewer,
        cwd=cwd,
        dry_run=dry_run,
    )
    typer.echo(result.receipt, err=True)
    raise typer.Exit(code=0 if result.ok or (dry_run and result.status == "skipped") else 1)


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
