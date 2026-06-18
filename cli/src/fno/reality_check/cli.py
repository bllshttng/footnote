"""fno reality-check subcommands - gh, notion, sheets."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer

cli = typer.Typer(name="reality-check", help="check external reality", no_args_is_help=True)


@cli.callback()
def _reality_check_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag
    merge_json_flag(ctx, json_output)


@cli.command()
def gh(
    ctx: typer.Context,
    pr: Optional[int] = typer.Option(None, "--pr-number", help="PR number to check"),
    pr_legacy: Optional[int] = typer.Option(
        None, "--pr", hidden=True, help="[DEPRECATED] alias for --pr-number."
    ),
    expect: str = typer.Option("open", "--expect", help="expected PR state (open/closed/merged)"),
    timeout: int = typer.Option(5, "--timeout", help="subprocess timeout in seconds"),
) -> None:
    """Check a GitHub PR state against an expected value."""
    import click

    from fno._flag_aliases import merge_deprecated_alias
    from fno.reality_check.gh import check_gh

    pr = merge_deprecated_alias(
        pr, pr_legacy, canonical_flag="--pr-number", legacy_flag="--pr"
    )
    # --pr-number is required; the merge returns None only when NEITHER
    # spelling was passed (the hidden alias forces a None default here).
    if pr is None:
        raise click.UsageError("Missing option '--pr-number'.")

    result = check_gh(pr_number=pr, expect=expect, timeout=timeout)
    typer.echo(json.dumps(result))
    if not result["ok"]:
        raise typer.Exit(code=1)


@cli.command()
def notion(
    ctx: typer.Context,
    target: Optional[str] = typer.Option(None, "--target", help="target identifier"),
) -> None:
    """Check Notion reality (stub - not yet implemented)."""
    from fno.reality_check.notion import check_notion

    result = check_notion(target=target)
    typer.echo(json.dumps(result))
    # Exit 0 intentionally - structured "not yet" is not a failure


@cli.command()
def sheets(
    ctx: typer.Context,
    target: Optional[str] = typer.Option(None, "--target", help="target identifier"),
) -> None:
    """Check Google Sheets reality (stub - not yet implemented)."""
    from fno.reality_check.sheets import check_sheets

    result = check_sheets(target=target)
    typer.echo(json.dumps(result))
    # Exit 0 intentionally - structured "not yet" is not a failure
