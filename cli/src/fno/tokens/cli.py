"""fno tokens - diagnose current Claude Code session token burn.

Thin wrapper around scripts/diagnostics/token-diagnose.py which is the source
of truth. The diagnose script analyzes the session transcript for cache
breaks, idle gaps, resume-bug indicators, and cost attribution.
"""
import subprocess
from pathlib import Path
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.paths import resolve_repo_root

app = typer.Typer(
    name="tokens",
    help="Diagnose token burn in the current Claude Code session.",
    invoke_without_command=True,
)


@app.callback()
def tokens(
    ctx: typer.Context,
    session_id: Optional[str] = typer.Argument(
        None, help="Session ID. Auto-detects current session if omitted."
    ),
    json: bool = typer.Option(False, "--json", "-J", help="Emit JSON instead of markdown."),
) -> None:
    """Run the token-diagnose script and propagate its exit code."""
    if ctx.invoked_subcommand is not None:
        return
    repo_root = Path(resolve_repo_root())
    script = repo_root / "scripts" / "diagnostics" / "token-diagnose.py"
    if not script.exists():
        typer.echo(f"token-diagnose script not found at {script}", err=True)
        raise typer.Exit(code=2)
    args = ["python3", str(script)]
    if json:
        args.append("--json")
    if session_id:
        args.append(session_id)
    try:
        result = subprocess.run(args)
    except FileNotFoundError as exc:
        typer.echo(f"failed to run token-diagnose: {exc}", err=True)
        raise typer.Exit(code=2)
    raise typer.Exit(code=propagate_returncode(result.returncode))
