"""fno agents worker subcommands: blueprint, ship, review, reconcile."""
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


@cli.command("blueprint-feed")
def blueprint_feed(
    ctx: typer.Context,
    scope: str = typer.Option(..., "--scope", help="Territory scope (canonical crown scope or project)."),
    deliver: bool = typer.Option(
        False, "--deliver", help="Mail each due idea to the standing worker as /fno:blueprint <id>."
    ),
    repair: Optional[str] = typer.Option(
        None, "--repair", help="Record a refusal/repair reason; the ideas stay preserved."
    ),
) -> None:
    """One territory's blueprinter feed: status, delivery, or repair receipt."""
    from fno.worker.blueprint import blueprint_feed as _feed

    result = _feed(scope, deliver=deliver, repair=repair)

    if _json_mode(ctx):
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result.get('action')}")
        typer.echo(f"scope: {result.get('scope')}")
        for idea in result.get("ideas", []):
            typer.echo(f"idea: {idea['id']} ({idea['rung']})")
        worker = result.get("worker")
        if worker:
            typer.echo(f"worker: {worker['name']} live={worker['live']}")


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

    if result.get("action") == "blocked":
        typer.echo(f"blocked: {result.get('error')}", err=True)
        raise typer.Exit(code=2)

    if _json_mode(ctx):
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result['action']}")
        typer.echo(f"pr_number: {result.get('pr_number')}")
        typer.echo(f"pr_url: {result.get('pr_url')}")


@cli.command()
def review() -> None:
    """Refuse: the sigma panel is removed; the review lane is bare /fno:review.

    No attestation is emitted here - a removed producer must never leave a
    path that still writes gate evidence (AC8-ERR).
    """
    typer.secho(
        "sigma is removed: the review is bare /fno:review (the fno review "
        "lane), which runs inline on every harness and emits its own "
        "attestation.",
        err=True,
    )
    raise typer.Exit(code=2)


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
