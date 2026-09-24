"""`fno backlog annotate` - one-release forwarding shim (x-26bd): each action
rewrites its argv to the `note` spelling, execs this same entrypoint, and one
stderr line names the replacement.
"""
from __future__ import annotations

import os
import sys

import typer

from fno.tombstones import tombstone_group_cls

# Three commands from the start (single-command sub-app collapse gotcha: a
# 1-command Typer flattens the verb away).
annotate_app = typer.Typer(
    no_args_is_help=True,
    help=(
        "Record an operator review finding against a node. The finding is a "
        "durable review_finding event loop-check gates on (blocks terminal-allow "
        "until resolved) AND a best-effort live-inject to the claim-holding "
        "session. add | list | resolve."
    ),
    cls=tombstone_group_cls("annotate"),
)


def _forward(new: list[str]) -> None:
    os.execvp(sys.argv[0], [sys.argv[0], *new])


@annotate_app.command("add")
def add(
    text: str = typer.Option(..., "--message", "-m", help="The annotation text."),
    node: str = typer.Option(..., "--node", help="The backlog node the finding is against."),
    block_cmd: str = typer.Option(None, "--block-cmd", help="The annotated block's command line."),
    block_excerpt_file: str = typer.Option(
        None,
        "--block-excerpt-file",
        help="Path to a file holding the block excerpt, or '-' to read it from stdin.",
    ),
) -> None:
    """Forward: `fno backlog note <node> "<text>" --blocking`."""
    typer.echo("annotate is retiring: use fno backlog note <node> \"<text>\" --blocking", err=True)
    extra = [node, text, "--blocking"]
    if block_cmd:
        extra += ["--block-cmd", block_cmd]
    if block_excerpt_file:
        extra += ["--block-excerpt-file", block_excerpt_file]
    _forward(["backlog", "note", *extra])


@annotate_app.command("list")
def list_cmd(
    node: str = typer.Option(None, "--node", help="Scope to one node. Omit for all."),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object per line instead of a summary."
    ),
) -> None:
    """Forward: `fno backlog notes findings [<node>]`."""
    typer.echo("annotate is retiring: use fno backlog notes findings [<node>]", err=True)
    extra = ["backlog", "notes", "findings"]
    if node:
        extra += ["--node", node]
    if as_json:
        extra.append("--json")
    _forward(extra)


@annotate_app.command("resolve")
def resolve(
    finding_id: str = typer.Argument(..., help="The finding id to resolve."),
) -> None:
    """Forward: `fno backlog note --resolve <finding-id>`."""
    typer.echo("annotate is retiring: use fno backlog note --resolve <finding-id>", err=True)
    _forward(["backlog", "note", "--resolve", finding_id])
