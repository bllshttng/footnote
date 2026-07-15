"""`fno mcp` CLI: thin wrappers over the MCP sidecar client.

Currently one verb, ``send``, used by the fno-agents daemon's push-channel
delivery leg to hand an envelope to a registered channel server. The envelope is
read from stdin (never argv) so it can be arbitrarily large.
"""

from __future__ import annotations

import json
import sys

import typer

mcp_app = typer.Typer(
    name="mcp",
    no_args_is_help=True,
    add_completion=False,
    help="MCP sidecar client verbs.",
)


@mcp_app.callback()
def _callback() -> None:
    """Keep ``send`` a real subcommand (Typer collapses a lone command)."""


@mcp_app.command("send")
def send(
    session: str = typer.Option(
        ..., "--session", help="Registered channel session id."
    ),
) -> None:
    """Route a JSON envelope (read from stdin) to the channel under SESSION.

    Exit 0 on delivery; 1 on a sidecar/delivery failure; 2 on a malformed
    envelope. The delivery leg lazy-starts the sidecar (client default).
    """
    from fno.mcp.client import (
        MCPSidecarError,
        MCPSidecarUnreachable,
        send_to_channel,
    )

    raw = sys.stdin.read()
    try:
        envelope = json.loads(raw)
    except json.JSONDecodeError as e:
        typer.echo(f"invalid envelope JSON: {e}", err=True)
        raise typer.Exit(2)
    if not isinstance(envelope, dict):
        typer.echo("envelope must be a JSON object", err=True)
        raise typer.Exit(2)

    try:
        send_to_channel(session, envelope)
    except MCPSidecarError as e:
        typer.echo(f"delivery failed: {e}", err=True)
        raise typer.Exit(1)
    except MCPSidecarUnreachable as e:
        typer.echo(f"sidecar unreachable: {e}", err=True)
        raise typer.Exit(1)
