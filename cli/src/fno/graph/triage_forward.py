"""The triage route the wheel keeps: every action answers on the native
door, so this module forwards instead of implementing."""

from __future__ import annotations

import subprocess

import typer


def _triage_forward(ctx: typer.Context) -> None:
    """The native door owns every triage action; the wheel keeps the route.

    The whole argv rides `fno-agents backlog triage`; its output is
    echoed back through the wheel and its exit code returned, so the
    front door lists the group and serves it on installs whose `fno`
    resolves to this wheel.

    The child gets `FNO_STATE_DIR` set to the state root the wheel's own
    graph resolver answers, so both legs read one store even when that
    root came from config or an in-process pin the child cannot see.
    """
    import os

    from fno import paths, rust_binary

    binary = rust_binary.resolve_binary()
    if binary is None:
        typer.echo(
            "Error: the triage group is served by the native door; no fno-agents binary found.",
            err=True,
        )
        raise typer.Exit(code=2)
    anchor = paths.graph_json().parent
    state_root = anchor.parent if anchor.name == "db" else anchor
    proc = subprocess.run(
        [str(binary), "backlog", "triage", *ctx.args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env={**os.environ, "FNO_STATE_DIR": str(state_root)},
    )
    if proc.stdout:
        typer.echo(proc.stdout, nl=False)
    if proc.stderr:
        typer.echo(proc.stderr, nl=False, err=True)
    raise typer.Exit(code=proc.returncode)


def register_triage_forward(cli) -> None:
    """Register the visible `triage` command on the backlog group."""
    cli.command(
        "triage",
        context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
        add_help_option=False,
    )(_triage_forward)
