"""`fno doctor scratch`: the scratch-shape sweep leaf (x-caf8).

The sweep lives in the fno-agents Rust binary; this leaf resolves the
installed binary and execs it. No Python fallback exists on purpose: the
classifier is the Rust port of the census rule table, and a second
implementation would drift from it.
"""

from __future__ import annotations

from typing import Optional

import typer

scratch_app = typer.Typer(help="Scratch-shape sweep over job tmp dirs (Rust runtime).")


def _route(verb: str, flags: list[str]) -> None:
    from fno.agents.rust_runtime import refuse_without_binary, route_to_rust, runtime_mode
    from fno.rust_binary import resolve_installed_binary

    binary = resolve_installed_binary()
    if runtime_mode() == "python" or binary is None:
        refuse_without_binary("scratch")
    # os.execv never returns; the binary prints the state words itself.
    route_to_rust(["scratch", verb, *flags], binary=binary)


@scratch_app.command("sweep")
def sweep(
    jobs_dir: Optional[str] = typer.Option(
        None, "--jobs-dir", help="Default: $CLAUDE_CONFIG_DIR/jobs, else ~/.claude/jobs."
    ),
    since_days: Optional[int] = typer.Option(
        None, "--since-days", help="Window in days (default config.evals.scratch_window_days, 28)."
    ),
    threshold: Optional[int] = typer.Option(
        None, "--threshold", help="Jobs at which a shape files one p1 node (default config.evals.scratch_threshold, 3)."
    ),
    dry_run: bool = typer.Option(
        False, "--dry-run", help="Print would-file lines; emit nothing, call nothing."
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Print the state lines as one JSON array."),
) -> None:
    """Walk job tmp dirs, classify authored scratch, file one p1 node past threshold."""
    flags: list[str] = []
    if jobs_dir:
        flags += ["--jobs-dir", jobs_dir]
    if since_days is not None:
        flags += ["--since-days", str(since_days)]
    if threshold is not None:
        flags += ["--threshold", str(threshold)]
    if dry_run:
        flags.append("--dry-run")
    if json_out:
        flags.append("--json")
    _route("sweep", flags)


@scratch_app.command("report")
def report(
    since_days: Optional[int] = typer.Option(None, "--since-days", help="Window in days (default 28)."),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the ranked table as JSON."),
) -> None:
    """Render the ranked scratch-shape table from the events journal."""
    flags: list[str] = []
    if since_days is not None:
        flags += ["--since-days", str(since_days)]
    if json_out:
        flags.append("--json")
    _route("report", flags)
