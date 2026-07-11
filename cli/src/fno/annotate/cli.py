"""`fno annotate` - record an operator review finding against a node.

Machine-first surface (mirrors `fno carveout`): the finding id prints to stdout,
the receipt line distinguishes ``recorded + delivered`` from ``recorded;
delivery deferred to bus`` (never a bare success - the one place a silent
failure could hide is inject-after-record). Exit codes: 0 ok / 2 invalid args.
"""
from __future__ import annotations

import json

import typer

from fno.annotate.core import (
    AnnotateError,
    add_finding,
    list_findings,
    resolve_finding,
)

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
)


@annotate_app.command("add")
def add(
    text: str = typer.Option(..., "--message", "-m", help="The annotation text."),
    node: str = typer.Option(..., "--node", help="The backlog node the finding is against."),
    block_cmd: str = typer.Option(None, "--block-cmd", help="The annotated block's command line."),
    block_excerpt_file: str = typer.Option(
        None, "--block-excerpt-file", help="Path to a file holding the block excerpt."
    ),
) -> None:
    """Record a finding and attempt live delivery to the claim holder.

    The event is the transaction; delivery is best-effort. Prints the finding id
    to stdout and a receipt line to stderr.
    """
    excerpt = None
    if block_excerpt_file:
        from pathlib import Path

        try:
            excerpt = Path(block_excerpt_file).read_text(encoding="utf-8")
        except OSError as exc:
            typer.echo(f"annotate: cannot read --block-excerpt-file: {exc}", err=True)
            raise typer.Exit(2)

    try:
        result = add_finding(node, text, block_cmd=block_cmd, block_excerpt=excerpt)
    except AnnotateError as exc:
        typer.echo(f"annotate: {exc}", err=True)
        raise typer.Exit(2)

    receipts = {
        "delivered": "recorded + delivered",
        "deferred": "recorded; delivery deferred to bus",
        "no-holder": "recorded; no live holder (gates the next worker)",
    }
    typer.echo(result["finding_id"])
    typer.echo(f"{result['node']}: {receipts[result['delivery']]}", err=True)


@annotate_app.command("list")
def list_cmd(
    node: str = typer.Option(None, "--node", help="Scope to one node. Omit for all."),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object per line instead of a summary."
    ),
) -> None:
    """List open + resolved findings (read-only). Missing log => nothing, exit 0."""
    findings = list_findings(node)
    if as_json:
        for f in findings:
            typer.echo(json.dumps(f, separators=(",", ":")))
        return
    for f in findings:
        state = "open" if f["open"] else "resolved"
        first = f["text"].splitlines()[0] if f["text"] else ""
        typer.echo(f"{f['finding_id']} [{state}] {f['node']}: {first}")


@annotate_app.command("resolve")
def resolve(
    finding_id: str = typer.Argument(..., help="The finding id to resolve."),
) -> None:
    """Resolve a finding. Idempotent: unknown/already-resolved is a warning no-op."""
    result = resolve_finding(finding_id)
    if result.get("warning"):
        typer.echo(f"annotate: {finding_id}: {result['warning']}", err=True)
        return
    typer.echo(f"{finding_id}: resolved")
