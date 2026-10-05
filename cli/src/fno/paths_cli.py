"""CLI surface for path introspection: fno config paths handoff.

emit-shell, shell-stub and verify answer natively
(crates/fno-agents/src/paths_cli.rs); this group still serves handoff until
its child node ports (one verb per PR, d-450caaeb).
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer

app = typer.Typer(
    name="paths",
    help="Path introspection and codegen for scripts/lib/paths.sh.",
    no_args_is_help=True,
)


@app.command(name="handoff")
def handoff(
    session_id: Optional[str] = typer.Option(
        None,
        "--session-id",
        help=(
            "Session id (full uuid or 8-hex short id). Names the canon "
            "handoff doc unless --slug overrides."
        ),
    ),
    slug: str = typer.Option(
        "",
        "--slug",
        help="Human-readable slug; overrides the session-derived short id in the filename.",
    ),
    scope: Optional[str] = typer.Option(
        None,
        "--scope",
        help=(
            "Crown scope: name the doc after the crown, not a session. Prints "
            "the newest existing handoff for that scope when one exists, so a "
            "successor session resolves its predecessor's doc; today's name "
            "when none does. Mutually exclusive with --session-id/--slug."
        ),
    ),
    name_only: bool = typer.Option(
        False, "--name-only", help="Print just the rendered filename, no directory."
    ),
) -> None:
    """Print the save path for a session's canon handoff doc.

    Backed by ``paths.handoffs_dir()``. The filename key is the session's mail
    handle (``canonical_handle``, the last-8 of the session id) unless --slug
    or --scope overrides. The PreCompact canon-doc hook and any session writing
    a handoff doc shell this instead of composing a path, so the configured
    location is the one door. A crowned session passes --scope: a crown
    outlives its sessions, so its rolling doc keys on the scope.
    """
    import datetime as _dt
    import re

    from fno.harness_identity import canonical_handle
    from fno.paths import handoffs_dir

    if scope:
        if session_id or slug:
            raise typer.BadParameter("--scope cannot be combined with --session-id/--slug")
        key = "crown-" + re.sub(r"[^A-Za-z0-9._-]+", "-", scope.strip()).strip("-")
        if key == "crown-":
            raise typer.BadParameter("a crown scope is required (--scope)")
        directory = handoffs_dir()

        def _mtime(path: Path) -> float:
            # A concurrent refresh can unlink between glob and stat; a vanished
            # candidate sorts oldest and the writer recreates the file anyway.
            try:
                return path.stat().st_mtime
            except OSError:
                return 0.0

        existing = sorted(directory.glob(f"*-{key}.md"), key=_mtime)
        filename = existing[-1].name if existing else f"{_dt.datetime.now().strftime('%Y%m%d')}-{key}.md"
        typer.echo(filename if name_only else str(directory / filename))
        return
    if not session_id:
        raise typer.BadParameter("a session id is required (--session-id), or a crown scope (--scope)")
    key = slug or canonical_handle(session_id)
    filename = f"{_dt.datetime.now().strftime('%Y%m%d')}-{key}.md"
    typer.echo(filename if name_only else str(handoffs_dir() / filename))
