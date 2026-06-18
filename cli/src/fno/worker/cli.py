"""fno worker subcommands: blueprint, ship, review, reconcile."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer

cli = typer.Typer(name="worker", help="manage delivery worker phases", no_args_is_help=True)


@cli.callback()
def _worker_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag
    merge_json_flag(ctx, json_output)


def _json_mode(ctx: typer.Context) -> bool:
    return bool(ctx.obj and ctx.obj.get("json", False))


@cli.command()
def blueprint(
    ctx: typer.Context,
    plan: str = typer.Option(..., "--plan", help="path to plan file or folder"),
) -> None:
    """Signal that LLM work is needed for the blueprint phase (does not write code)."""
    from fno.worker.blueprint import blueprint as _blueprint

    result = _blueprint(plan_path=plan)

    if _json_mode(ctx):
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result['action']}")
        typer.echo(f"plan_path: {result['plan_path']}")
        typer.echo(f"next_step: {result['next_step']}")


@cli.command()
def ship(
    ctx: typer.Context,
    title: str = typer.Option("", "--title", help="PR title"),
    body: str = typer.Option("", "--body", help="PR body"),
    state: Optional[Path] = typer.Option(None, "--state", help="path to target-state.md"),
    artifacts_dir: Optional[Path] = typer.Option(None, "--artifacts-dir", help="artifacts directory"),
    base_branch: str = typer.Option("main", "--base", help="base branch for PR"),
) -> None:
    """Create or detect an existing PR idempotently, write ship artifact."""
    from fno.worker.ship import ship as _ship

    state_path = state or Path(".fno/target-state.md")
    if not state_path.exists():
        typer.echo(f"error: state file not found: {state_path}", err=True)
        raise typer.Exit(code=3)

    result = _ship(
        state_path=state_path,
        title=title,
        body=body,
        artifacts_dir=artifacts_dir,
        base_branch=base_branch,
    )

    if result.get("action") == "error":
        typer.echo(f"error: {result.get('error')}", err=True)
        raise typer.Exit(code=2)

    if _json_mode(ctx):
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result['action']}")
        typer.echo(f"pr_number: {result.get('pr_number')}")
        typer.echo(f"pr_url: {result.get('pr_url')}")
        if result.get("auto_merge_armed"):
            typer.echo("auto_merge: armed")


@cli.command()
def review(
    ctx: typer.Context,
    session: Optional[str] = typer.Option(None, "--session-id", help="session id (overrides state file)"),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    state: Optional[Path] = typer.Option(None, "--state", help="path to target-state.md"),
    diff: Optional[Path] = typer.Option(None, "--diff", help="path to diff file (default: git diff HEAD~1)"),
    artifacts_dir: Optional[Path] = typer.Option(None, "--artifacts-dir", help="artifacts directory"),
    no_cache: bool = typer.Option(False, "--no-cache", help="bypass cache read and write"),
) -> None:
    """Run internal sigma-review orchestrator and write quality_check artifact."""
    import json
    import subprocess
    from fno._flag_aliases import merge_deprecated_alias
    from fno.worker.review import review as _review
    from fno.review.locking import ReviewLockBusy

    session = merge_deprecated_alias(
        session, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )

    state_path = state or Path(".fno/target-state.md")

    # Read diff from file or git
    if diff is not None:
        diff_context = diff.read_text(encoding="utf-8")
    else:
        git_result = subprocess.run(
            ["git", "diff", "HEAD~1"],
            capture_output=True,
            text=True,
        )
        diff_context = git_result.stdout if git_result.returncode == 0 else ""

    try:
        result = _review(
            diff_context=diff_context,
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session,
            no_cache=no_cache,
        )
    except ReviewLockBusy as exc:
        typer.echo(f"error: review lock busy: {exc}", err=True)
        raise typer.Exit(code=11)

    if _json_mode(ctx):
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result['action']}")
        typer.echo(f"verdict: {result.get('verdict', 'unknown')}")
        typer.echo(f"findings: {result.get('findings', 0)}")
        if result.get("cached"):
            typer.echo("cached: true")


@cli.command()
def external(
    ctx: typer.Context,
    pr: Optional[int] = typer.Option(None, "--pr-number", help="PR number to poll"),
    pr_legacy: Optional[int] = typer.Option(
        None, "--pr", hidden=True, help="[DEPRECATED] alias for --pr-number."
    ),
    state: Optional[Path] = typer.Option(None, "--state", help="path to target-state.md"),
) -> None:
    """Poll for external review status on a PR (GitHub)."""
    from fno._flag_aliases import merge_deprecated_alias
    from fno.worker.external import external_review

    pr = merge_deprecated_alias(
        pr, pr_legacy, canonical_flag="--pr-number", legacy_flag="--pr"
    )
    state_path = state or Path(".fno/target-state.md")
    result = external_review(pr_number=pr, state_path=state_path)

    if _json_mode(ctx):
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result['action']}")
        if result.get("next_check_in"):
            typer.echo(f"next_check_in: {result['next_check_in']}")


@cli.command()
def reconcile(
    ctx: typer.Context,
    scan: bool = typer.Option(False, "--scan", help="scan for orphaned PRs"),
    state: Optional[Path] = typer.Option(None, "--state", help="path to target-state.md"),
) -> None:
    """Detect merged/orphaned PRs and update state + graph atomically."""
    from fno.worker.reconcile import reconcile as _reconcile

    state_path = state or Path(".fno/target-state.md")
    result = _reconcile(state_path=state_path, scan=scan)

    if _json_mode(ctx):
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result['action']}")
        if result.get("pr_number"):
            typer.echo(f"pr_number: {result['pr_number']}")
