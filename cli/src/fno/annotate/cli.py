from __future__ import annotations

import typer

from fno.tombstones import tombstone_group_cls

# retired-ok: refusal messages naming the replacement are the gate's own
# sanctioned shape (scripts/ci/check-retired-command-strings.sh, narrowing 3).
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
    typer.echo("annotate is retired: use fno backlog note <node> \"<text>\" --blocking", err=True)
    raise typer.Exit(code=2)


@annotate_app.command("list")
def list_cmd(
    node: str = typer.Option(None, "--node", help="Scope to one node. Omit for all."),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object per line instead of a summary."
    ),
) -> None:
    typer.echo("annotate is retired: use fno backlog notes findings [<node>]", err=True)
    raise typer.Exit(code=2)


@annotate_app.command("resolve")
def resolve(
    finding_id: str = typer.Argument(..., help="The finding id to resolve."),
) -> None:
    typer.echo("annotate is retired: use fno backlog note --resolve <finding-id>", err=True)
    raise typer.Exit(code=2)
