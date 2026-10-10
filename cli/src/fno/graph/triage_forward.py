"""The triage route the wheel keeps: every action answers on the native
door, so this module forwards instead of implementing."""

from __future__ import annotations

import subprocess

import typer


def _triage_forward(ctx) -> None:
    """The native door owns every triage action; the wheel keeps the route.

    The whole argv rides `fno-agents backlog triage` with stdio inherited
    and its exit code returned, so the front door lists the group and
    serves it on installs whose `fno` resolves to this wheel.
    """
    import subprocess

    from fno import rust_binary

    binary = rust_binary.resolve_binary()
    if binary is None:
        typer.echo(
            "Error: the triage group is served by the native door; no fno-agents binary found.",
            err=True,
        )
        raise typer.Exit(code=2)
    proc = subprocess.run([str(binary), "backlog", "triage", *ctx.args])
    raise typer.Exit(code=proc.returncode)


def register_triage_forward(cli) -> None:
    """Register the visible `triage` command on the backlog group."""
    cli.command(
        "triage",
        context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    )(_triage_forward)
