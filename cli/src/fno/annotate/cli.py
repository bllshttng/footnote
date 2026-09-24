"""`fno backlog annotate`: one-release forwarding shim.

`note --blocking` replaced annotate (x-26bd): one verb, one store, and the
finding is gate state instead of a journal line a relative path could lose.
Each action forwards and prints one stderr line naming the replacement; the
shim is removed one release out.
"""
from __future__ import annotations

import subprocess
from typing import Optional

import typer

from fno.tombstones import tombstone_group_cls

# Three commands kept so the retiring surface still answers its old shape.
annotate_app = typer.Typer(
    no_args_is_help=True,
    help=(
        "Retiring: `fno backlog note <node> \"<text>\" --blocking` records a "
        "finding, `fno backlog notes findings [<node>]` reads them, and `fno "
        "backlog note --resolve <id>` clears one. This shim forwards one release."
    ),
    cls=tombstone_group_cls("annotate"),
)


def _retire_line(replacement: str) -> None:
    typer.echo(f"annotate is retiring: use {replacement}", err=True)


@annotate_app.command("add")
def add(
    text: str = typer.Option(..., "--message", "-m", help="The finding text."),
    node: str = typer.Option(..., "--node", help="The backlog node the finding is against."),
    block_cmd: str = typer.Option(None, "--block-cmd", help="The annotated block's command line."),
    block_excerpt_file: str = typer.Option(
        None,
        "--block-excerpt-file",
        help="Path to a file holding the block excerpt, or '-' to read it from stdin.",
    ),
) -> None:
    """Forward to `fno backlog note <node> "<text>" --blocking`."""
    _retire_line('fno backlog note <node> "<text>" --blocking')
    from fno.graph.note_cli import cmd_note

    cmd_note(
        task_id=node,
        text=text,
        body_file=None,
        quiet=False,
        json_output=False,
        read=[],
        blocking=True,
        resolve=None,
        block_cmd=block_cmd,
        block_excerpt_file=block_excerpt_file,
    )


@annotate_app.command("list")
def list_cmd(
    node: str = typer.Option(None, "--node", help="Scope to one node. Omit for all."),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object per finding."
    ),
) -> None:
    """Forward to `fno backlog notes findings`."""
    _retire_line("fno backlog notes findings [<node>]")
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("Error: the fno-agents binary is required for `fno backlog annotate list`", err=True)
        raise typer.Exit(code=2)
    argv = [str(binary), "backlog-notes", "findings"]
    if node:
        argv.extend(["--node", node])
    if as_json:
        argv.append("--json")
    proc = subprocess.run(argv, check=False)
    raise typer.Exit(code=propagate_returncode(proc.returncode))


@annotate_app.command("resolve")
def resolve(
    finding_id: str = typer.Argument(..., help="The finding id to resolve."),
) -> None:
    """Forward to `fno backlog note --resolve <finding-id>`."""
    _retire_line("fno backlog note --resolve <finding-id>")
    from fno.graph.note_cli import cmd_note

    cmd_note(
        task_id=None,
        text=None,
        body_file=None,
        quiet=False,
        json_output=False,
        read=[],
        blocking=False,
        resolve=finding_id,
        block_cmd=None,
        block_excerpt_file=None,
    )
