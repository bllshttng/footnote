"""fno do pr CLI - in-package gh/git PR operations (ab-d4c98550).

Verbs:
    merge  - merge a PR with the fno-canonical guards (-> _merge.py)
    verify - audit an external PR gate, merged|reviews (-> _verify.py)
    rebase - two-phase rebase with conflict delegation (-> _rebase.py)
    logs   - tail the failing CI job, spool the rest (-> _logs.py)
    wait   - poll status through the coalescing cache until settled/green (-> _wait.py)

The four ``scripts/lib/pr-*.sh`` were ported to in-package Python shelling to
gh/git, so these verbs run from a bare ``pip install fno`` with no repo-root
dependency. Each module preserves the bash exit-code / output contract.
"""

from __future__ import annotations

import enum
import json
import os
from typing import List, Optional

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


class GraphqlPurpose(str, enum.Enum):
    discretionary = "discretionary"


attestation_app = typer.Typer(
    name="attestation",
    help="Retraction for review attestations (the event, not the identity).",
    no_args_is_help=True,
    add_completion=False,
)


@attestation_app.command(
    "retract",
    hidden=True,
    help=(
        "Revoke a passing review_attestation by naming the pair that passed: "
        "--reviewer, --attester (the session that emitted the pass), --head. "
        "Writes a fail verdict carrying retracts_attester and records the "
        "RETRACTING process's own identity. Refuses when no matching pass "
        "exists, so a revocation never lands for a pair that never passed."
    ),
)
def attestation_retract(
    reviewer: str = typer.Option(..., "--reviewer", help="Reviewer name of the pass being revoked."),
    attester: str = typer.Option(..., "--attester", help="The session id that emitted the pass."),
    head: str = typer.Option(..., "--head", help="The head sha the pass pinned."),
    reason: str = typer.Option(..., "--reason", "-R", help="Why the pass is revoked (recorded)."),
    events: Optional[str] = typer.Option(
        None, "--events", help="Path to events.jsonl (default: the repo project log)."
    ),
) -> None:
    from pathlib import Path

    from fno.pr import _attestation

    rc = _attestation.retract(
        reviewer,
        attester,
        head,
        reason,
        events=Path(events) if events else None,
    )
    raise typer.Exit(code=rc)


pr_app.add_typer(attestation_app, name="attestation", hidden=True)


@pr_app.command(
    "merge",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Merge a PR via gh CLI with the fno-canonical guards (<pr_number>). "
        "Emits a JSON line {pr, outcome, reason, strategy[, cleanup]}; exit 0 merged, "
        "1 failed, 2 skipped|held, 3 merge-landed-cleanup-failed, 127 gh-missing."
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
        "reads red AND unsettled. checks.total counts the whole rollup: "
        "checks.check_runs and checks.statuses split it, because "
        "`gh api .../check-runs` returns only the first kind. A served answer "
        "carries cached, cached_at and cached_age_seconds beside the head "
        "it was computed at; "
        "--refresh bypasses the cache for one live read."
    ),
)
def status(
    pr_number: int = typer.Argument(..., help="GitHub PR number"),
    refresh: bool = typer.Option(
        False,
        "--refresh",
        "--no-cache",
        help=(
            "Bypass the coalescing cache and read GitHub live. Manual use "
            "only - it defeats the coalescing that keeps a watcher fleet "
            "under the REST secondary limit, so never put it in a poll loop."
        ),
    ),
) -> None:
    from fno.pr import _status

    # main() routes through the coalescing cache: the watcher
    # recipe polls this verb every 60s per session, and N sessions polling one
    # PR must collapse to one network read per TTL or they trip the REST
    # secondary limit (which counts request rate, not budget).
    rc = _status.main([str(pr_number)] + (["--refresh"] if refresh else []))
    raise typer.Exit(code=rc)


@pr_app.command(
    "wait",
    hidden=True,
    help=(
        "The sanctioned watcher: poll `fno do pr status` through the "
        "coalescing cache until the PR settles (or turns green with "
        "--until green), then exit the status verb's own code. N waiters on "
        "one PR cost one network read per cache TTL, a rate-limit backoff is "
        "ridden out rather than hammered, and the gh-call count prints at "
        "exit. --timeout (30m default) exits with the last observed code and "
        "a still-unsettled note. Use this instead of a hand-rolled "
        "`while/sleep/grep` loop - every such loop is an uncoordinated poll "
        "against a quota the whole machine shares."
    ),
)
def wait(
    pr_number: int = typer.Argument(..., help="GitHub PR number"),
    until: str = typer.Option(
        "settled", "--until", help="Exit when: settled (any terminal verdict) or green."
    ),
    timeout: str = typer.Option("30m", "--timeout", help="Max wait, e.g. 30m / 90s / 1h."),
    interval: str = typer.Option("60", "--interval", help="Poll interval in seconds (minimum 5)."),
) -> None:
    from fno.pr import _wait

    # No ToolMissing handler here: `_wait.main` maps it to 127 itself, and a
    # second handler for the same exception is a copy that drifts.
    rc = _wait.main(
        [
            str(pr_number),
            "--until",
            until,
            "--timeout",
            timeout,
            "--interval",
            interval,
        ]
    )
    raise typer.Exit(code=rc)


@pr_app.command(
    "coverage-publish",
    hidden=True,
    help=(
        "Publish the review-coverage verdict for a PR head as the commit "
        "status fno/review-coverage (the context the repo ruleset requires). "
        "Never recomputes: reads the row the emitters already wrote. "
        "Prints a JSON line {pr, posted, note}; exit 0 posted, 1 not posted."
    ),
)
def coverage_publish(
    pr_number: int = typer.Argument(..., help="GitHub PR number"),
    head: Optional[str] = typer.Option(
        None, "--head", help="Head sha to publish for; default: the PR's headRefOid."
    ),
    repo: Optional[str] = typer.Option(
        None, "--repo", help="Directory whose git remote names the repository."
    ),
) -> None:
    from fno.pr import _reviews

    posted, note = _reviews.publish_coverage_status(pr_number, head, cwd=repo, repo=repo)
    typer.echo(
        json.dumps(
            {"pr": pr_number, "posted": posted, "note": note},
            separators=(",", ":"),
        )
    )
    raise typer.Exit(code=0 if posted else 1)


@pr_app.command(
    "info",
    help=(
        "Read PR state, URL, head SHA, refs, and mergeability through one REST request. "
        "Prints JSON; exit 4 when the REST instrument cannot answer."
    ),
)
def info(
    pr_number: Optional[int] = typer.Argument(
        None, help="GitHub PR number; defaults to current branch"
    ),
    repo: Optional[str] = typer.Option(
        None, "--repo", help="GitHub owner/repo; defaults to origin"
    ),
) -> None:
    from fno.pr import _rest

    if pr_number is None:
        pr_number, reason = _rest.resolve_current_pr_number_rest(cwd=os.getcwd(), repo=repo)
        if pr_number is None:
            typer.echo(json.dumps({"pr": None, "error": reason}, separators=(",", ":")))
            raise typer.Exit(code=4)
    payload, reason = _rest.fetch_pr_info_rest(str(pr_number), cwd=os.getcwd(), repo=repo)
    if payload is None:
        typer.echo(json.dumps({"pr": pr_number, "error": reason}, separators=(",", ":")))
        raise typer.Exit(code=4)
    typer.echo(json.dumps(payload, separators=(",", ":")))


@pr_app.command(
    "list",
    help="List PR number, state, title, head ref, and URL through REST.",
)
def list_cmd(
    state: str = typer.Option("open", "--state", help="open, closed, or all"),
    repo: Optional[str] = typer.Option(
        None, "--repo", help="GitHub owner/repo; defaults to origin"
    ),
) -> None:
    from fno.pr import _rest

    if state not in {"open", "closed", "all"}:
        typer.echo(json.dumps({"error": "--state must be open, closed, or all"}))
        raise typer.Exit(code=2)
    slug = repo or _rest._repo_slug(os.getcwd())
    if not slug:
        typer.echo(json.dumps({"error": "could not resolve owner/repo"}))
        raise typer.Exit(code=4)
    rows, reason = _rest.list_prs_rest(slug, state=state, cwd=os.getcwd(), details=True)
    if rows is None:
        typer.echo(json.dumps({"error": reason}, separators=(",", ":")))
        raise typer.Exit(code=4)
    open_rows = [row for row in rows if row.get("state") == "OPEN"]
    if open_rows:
        from fno.graph._reconcile import classify_open_pr_bindings
        from fno.graph.store import read_graph_strict
        from fno.paths import graph_json

        try:
            bindings = classify_open_pr_bindings(open_rows, read_graph_strict(graph_json()))
        except Exception as exc:  # noqa: BLE001 - report the unreadable binding source
            for row in open_rows:
                row["node_binding_error"] = f"graph binding read failed: {exc}"
        else:
            by_pr = {binding.pr_number: binding for binding in bindings}
            for row in rows:
                number = row.get("number")
                if not isinstance(number, int):
                    continue
                binding = by_pr.get(number)
                if binding is None:
                    continue
                row["node_id"] = binding.node_id
                row["node_binding"] = binding.verdict
    typer.echo(json.dumps(rows, separators=(",", ":")))


@pr_app.command(
    "graphql-exec",
    hidden=True,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help="Run one gh GraphQL read under the reserve accounting.",
    epilog=(
        "Everything after -- is passed to gh verbatim as its argv, command "
        "words first: fno do pr graphql-exec --purpose discretionary -- api "
        "graphql -f query=... --jq ... . A flag-first argv is refused, "
        "because gh answers flags with no command word by printing its root "
        "command list."
    ),
)
def graphql_exec(
    ctx: typer.Context,
    purpose: GraphqlPurpose = typer.Option(..., "--purpose"),
) -> None:
    from fno.pr import _quota

    result = _quota.execute_graphql(purpose.value, list(ctx.args))
    if result.stdout:
        typer.echo(result.stdout, nl=False)
    if result.stderr:
        typer.echo(result.stderr, err=True)
    raise typer.Exit(code=result.returncode)


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
        typer.echo(f"fno do pr logs: {exc.tool} not found on PATH", err=True)
        rc = 127
    raise typer.Exit(code=rc)


@pr_app.command(
    "base-check",
    help=(
        "Refuse a PR whose branch base is > 24h of main history behind "
        "origin/main (phantom-deletion guard). Exit 0 fresh|bypass|fail-open, "
        "3 stale (points at `fno do pr rebase`), 4 unrelated histories. Bypass "
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
        typer.echo(f"fno do pr base-lineage-check: {exc.tool} not found on PATH", err=True)
        rc = 127
    raise typer.Exit(code=rc)


@pr_app.command(
    "coverage-check",
    hidden=True,
    help=(
        "The merge guard's coverage predicate, for callers that cannot import "
        "it (the stdlib-only git-protection hook shells this verb on a bare "
        "gh pr merge). Exit 0 covered, 3 uncovered (the guard's refusal on "
        "stderr), 4 unanswered (a named instrument failure), 5 impossible "
        "(round budget spent with blocking findings non-terminal; the "
        "refusal names the two remedies that can still clear it)."
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
    "review-hold",
    hidden=True,
    help=(
        "Register, clear, or read the hold that says a review of this PR's head "
        "is RUNNING. Merge readiness only knows what verdicts EXIST for a head, "
        "so a review still writing its fixes is invisible to it. Actions: "
        "check <pr> (exit 0 clear, 3 held, 4 unanswered), acquire, release."
    ),
)
def review_hold(
    action: str = typer.Argument(..., help="check | acquire | release"),
    pr_number: Optional[int] = typer.Argument(None, help="GitHub PR number (check)"),
    branch: Optional[str] = typer.Option(None, "--branch", help="Head branch (acquire/release)."),
    head: str = typer.Option("", "--head", help="The head sha being reviewed (acquire)."),
    holder: Optional[str] = typer.Option(None, "--holder", help="Who holds the review."),
    verb: Optional[str] = typer.Option(None, "--verb", help="Which review verb is running."),
    invocation_id: Optional[str] = typer.Option(
        None, "--invocation-id", help="Review invocation telemetry join id."
    ),
    args_raw: Optional[str] = typer.Option(
        None, "--args-raw", help="Review arguments preserved for invocation telemetry."
    ),
    level: Optional[str] = typer.Option(None, "--level", help="Parsed review level."),
    level_source: Optional[str] = typer.Option(
        None, "--level-source", help="Source of the parsed review level."
    ),
    flags_json: Optional[str] = typer.Option(
        None, "--flags-json", help="JSON array of parsed review flags."
    ),
    ttl_minutes: Optional[float] = typer.Option(
        None, "--ttl-minutes", help="Override config.review.hold_ttl_minutes."
    ),
    repo: Optional[str] = typer.Option(None, "--repo", help="Repository working directory."),
) -> None:
    from fno.pr import _review_hold

    cwd = repo or os.getcwd()
    if action == "check":
        if pr_number is None:
            typer.echo("review-hold check needs a PR number", err=True)
            raise typer.Exit(code=1)
        from fno.pr._merge import _pr_head_ref_and_oid

        refs = _pr_head_ref_and_oid(pr_number, cwd)
        if refs is None:
            # Exit 4 is the named-instrument-failure code `coverage-check` uses,
            # and a caller that cannot import fno reads it the same way: the
            # probe died, so this is not a verdict.
            typer.echo(
                f"PR {pr_number}: could not resolve the head branch; no in-flight "
                "review could be ruled out",
                err=True,
            )
            raise typer.Exit(code=4)
        branch, head, _state = refs
        refusal = _review_hold.review_hold_refusal(branch, pr_head=head, repo=cwd)
        if refusal:
            typer.echo(refusal, err=True)
            raise typer.Exit(code=3)
        typer.echo(f"PR {pr_number}: no review in flight on {branch}")
        raise typer.Exit(code=0)

    if not branch:
        typer.echo(f"review-hold {action} needs --branch", err=True)
        raise typer.Exit(code=1)
    if action == "release":
        # --holder is OPTIONAL here, and omitting it is the normal case: this is
        # a lane lock, not an ownership assertion. The refusal an operator reads
        # prints exactly `--branch <b>`, so requiring more would make the one
        # documented recovery command exit 1.
        released = _review_hold.release_review_hold(branch, holder=holder)
        # Say which happened. "released" printed over an absent hold is the
        # absence-as-success shape, on the one recovery path there is.
        typer.echo(
            f"review-hold: released {branch}" if released else f"review-hold: no hold on {branch}"
        )
        raise typer.Exit(code=0)
    if action == "metadata":
        from fno.claims.core import claim_status

        typer.echo(json.dumps(claim_status(_review_hold.review_hold_key(branch))))
        raise typer.Exit(code=0)
    if not holder:
        typer.echo("review-hold acquire needs --holder", err=True)
        raise typer.Exit(code=1)
    if action == "acquire":
        flags = None
        if flags_json is not None:
            try:
                parsed_flags = json.loads(flags_json)
            except json.JSONDecodeError:
                parsed_flags = None
            if isinstance(parsed_flags, list) and all(isinstance(flag, str) for flag in parsed_flags):
                flags = parsed_flags
        claim = _review_hold.acquire_review_hold(
            branch,
            head=head,
            holder=holder,
            verb=verb,
            invocation_id=invocation_id,
            args_raw=args_raw,
            level=level,
            level_source=level_source,
            flags=flags,
            ttl_ms=(_review_hold.resolve_ttl_ms(ttl_minutes) if ttl_minutes is not None else None),
        )
        if claim is None:
            # Registration never blocks a review from starting: an unheld review
            # is still covered by the worktree layer, and a review that refuses
            # to start because a lockfile write failed is strictly worse.
            typer.echo(f"review-hold: not registered on {branch}", err=True)
            raise typer.Exit(code=0)
        typer.echo(f"review-hold: holding {branch} at {head or 'an unrecorded head'}")
        raise typer.Exit(code=0)
    typer.echo(f"unknown review-hold action: {action}", err=True)
    raise typer.Exit(code=1)


@pr_app.command("hold-check", hidden=True)
def hold_check(
    pr_number: int = typer.Argument(..., help="GitHub PR number"),
    repo: Optional[str] = typer.Option(None, "--repo", help="Repository working directory."),
) -> None:
    """Refuse a PR whose bound plan ancestry carries an active or unreadable hold."""
    from fno.pr._hold import merge_hold_reason

    reason = merge_hold_reason(pr_number, repo or os.getcwd())
    if reason:
        typer.echo(reason, err=True)
        raise typer.Exit(code=3)
    typer.echo(f"PR {pr_number}: no plan dispatch hold")


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

    raise typer.Exit(code=_preflight.run_evidence_check(allow_equivalent=allow_rebase_equivalent))


@pr_app.command("evidence-required", hidden=True)
def evidence_required(
    base: str = typer.Option("origin/main", "--base"),
) -> None:
    """Expose the one local-verification policy to shell-based ship paths."""
    from fno.pr import _preflight

    required, reason = _preflight.local_verification_required(cwd=os.getcwd(), base_ref=base)
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
        "`fno do pr merged` skill is the attended front door."
    ),
)
def ritual(
    pr_number: Optional[int] = typer.Argument(
        None, help="Merged PR number; omitted -> most recently merged PR for this repo."
    ),
    autonomous: bool = typer.Option(
        False,
        "--autonomous",
        help="No operator present: spawn the judgment leg as one headless one-shot "
        "when its inputs are non-empty. Mirrors the POST_MERGE_NONINTERACTIVE=1 env.",
    ),
) -> None:
    from fno.pr import _ritual

    if os.environ.get("POST_MERGE_NONINTERACTIVE", "") == "1":
        autonomous = True
    rc = _ritual.run_ritual(pr_number, autonomous)
    raise typer.Exit(code=rc)


@pr_app.command(
    "closure-trailer",
    help=(
        "Print the exact `Backlog-Closure:` trailer for NODE plus its "
        "contained_in descendants. Compose it into a PR body before "
        "`gh pr create` so every node the PR ships gets bound at merge, not "
        "just the one stamped by --pr-number. Prints nothing (exit 0) when "
        "NODE is unresolvable or nothing well-formed remains, so a caller "
        "can append the output to a body unconditionally."
    ),
)
def closure_trailer(
    node: str = typer.Argument(..., help="Node id to render the trailer for."),
    extra: List[str] = typer.Option(
        [],
        "--extra",
        help="Additional genuinely-shipped node ids beyond NODE and its "
        "contained_in descendants (repeatable).",
    ),
) -> None:
    from fno.graph.store import read_graph
    from fno.paths import graph_json
    from fno.pr.closure import render_pr_closure_trailer
    from fno.tracker import active_backend_name

    from fno.graph._constants import is_wellformed_node_id

    if active_backend_name() != "graph":
        # graph.json is not the delivery record of truth under an external
        # tracker backend - nothing to render from, matching this command's
        # own contract (prints nothing, exit 0, on any unresolvable input).
        return

    try:
        entries = read_graph(graph_json())
    except Exception:
        return
    # render_pr_closure_trailer silently drops a malformed id with no other
    # signal - a bare-hex or slug typo in --extra would otherwise ship with
    # the trailer one node short and no one the wiser (round-7 review fix).
    dropped = [e for e in extra if not is_wellformed_node_id(e)]
    if dropped:
        typer.echo(
            f"warning: dropping malformed --extra id(s) from the trailer: "
            f"{', '.join(dropped)} (need the full <prefix>-<hex> form)",
            err=True,
        )
    line = render_pr_closure_trailer(entries, node, extra_ids=list(extra))
    if line:
        typer.echo(line)


@pr_app.command("bind-created", hidden=True)
def bind_created(
    url: str = typer.Option(..., "--url", help="Created PR URL."),
    owner: Optional[str] = typer.Option(None, "--owner", help="Best-known live owner."),
    repo: Optional[str] = typer.Option(None, "--repo", help="Repository worktree."),
    node: Optional[str] = typer.Option(
        None,
        "--node",
        help="Authoritative node id (the manifest's). Leads; branch is the fallback.",
    ),
) -> None:
    """Bind a raw ``gh pr create`` result to its one real node."""
    from fno.pr.closure import bind_created_pr_from_branch

    result = bind_created_pr_from_branch(url, owner=owner, cwd=repo or os.getcwd(), node_id=node)
    if result.outcome == "bound":
        # The ship row follows the binding, not one particular verb. This used
        # to ride on `backlog update --pr-number`; when ship switched to this
        # command the stamp silently stopped happening for every PR it opened.
        from fno.graph.cli import _stamp_ship_on_pr_link

        for bound_id in result.bound_ids:
            _stamp_ship_on_pr_link(bound_id)
    typer.echo(
        json.dumps(
            {
                "outcome": result.outcome,
                "claimed_ids": result.claimed_ids,
                "refusal": result.refusal,
            },
            separators=(",", ":"),
        )
    )
    raise typer.Exit(code=0 if result.outcome == "bound" else 1)
