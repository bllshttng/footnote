"""CLI surface for path introspection: fno paths emit-shell / fno paths verify."""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer

app = typer.Typer(
    name="paths",
    help="Path introspection and codegen for scripts/lib/paths.sh.",
    no_args_is_help=True,
)


@app.command(name="emit-shell")
def emit_shell(
    output: Optional[Path] = typer.Option(
        None,
        "--output",
        help=(
            "Destination file path. Defaults to scripts/lib/paths.sh "
            "relative to the repo root."
        ),
    ),
) -> None:
    """Generate scripts/lib/paths.sh from the Pydantic schema.

    The generated file is byte-deterministic for identical schema inputs.
    Use --output to redirect to a custom path (useful for tests).
    """
    from fno.paths import resolve_repo_root
    from fno.setup.emit_shell import emit_paths_sh
    from fno.state.io import atomic_write

    if output is None:
        repo_root = resolve_repo_root()
        output = repo_root / "scripts" / "lib" / "paths.sh"

    output.parent.mkdir(parents=True, exist_ok=True)

    content = emit_paths_sh(use_defaults=True)
    atomic_write(output, content)
    typer.echo(f"wrote {len(content.encode('utf-8'))} bytes to {output}")


@app.command(name="shell-stub")
def shell_stub() -> None:
    """Generate a fresh paths.sh from current settings and print its path.

    Bash callers use: source "$(fno paths shell-stub)".

    Each invocation regenerates a temp file from the current settings.yaml so
    shell hooks always reflect the user's current config rather than the
    checked-in static snapshot.  The checked-in scripts/lib/paths.sh remains
    available as a fallback for callers where fno is not on PATH.
    """
    import tempfile
    from fno.setup.emit_shell import emit_paths_sh

    content = emit_paths_sh(use_defaults=False)
    with tempfile.NamedTemporaryFile(
        mode="w",
        suffix=".sh",
        prefix="fno-paths-",
        delete=False,
        encoding="utf-8",
    ) as f:
        f.write(content)
        print(f.name)


@app.command(name="verify")
def verify_cmd(
    paths_sh: Optional[Path] = typer.Argument(
        None,
        help=(
            "Path to scripts/lib/paths.sh. "
            "Defaults to scripts/lib/paths.sh relative to the repo root."
        ),
    ),
) -> None:
    """Verify that scripts/lib/paths.sh matches the schema-derived hash.

    Exits 0 if in sync, non-zero with a diff and regen command if not.
    """
    from fno.paths import resolve_repo_root
    from fno.paths_verify import verify

    if paths_sh is None:
        repo_root = resolve_repo_root()
        paths_sh = repo_root / "scripts" / "lib" / "paths.sh"

    if not paths_sh.exists():
        typer.echo(
            f"error: {paths_sh} does not exist. "
            "Generate it with: uv run fno-py paths emit-shell",
            err=True,
        )
        raise typer.Exit(code=1)

    ok, derived, checked = verify(paths_sh)

    if ok:
        typer.echo(f"paths.sh is in sync with schema (hash: {derived[:12]}...)")
    else:
        typer.echo(
            f"--- expected (from schema)\n"
            f"+++ checked-in\n"
            f"schema hash:  {derived}\n"
            f"file hash:    {checked}\n"
            f"\nHashes differ. Regenerate with:\n"
            f"  cd cli && uv run fno-py paths emit-shell",
            err=True,
        )
        raise typer.Exit(code=1)


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
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    slug: str = typer.Option(
        "",
        "--slug",
        help="Human-readable slug; overrides the session-derived short id in the filename.",
    ),
    name_only: bool = typer.Option(
        False, "--name-only", help="Print just the rendered filename, no directory."
    ),
) -> None:
    """Print the save path for a session's canon handoff doc.

    Backed by ``paths.handoffs_dir()``. The filename key is the session's mail
    handle (``canonical_handle``, the last-8 of the session id) unless --slug
    overrides. The PreCompact canon-doc hook and any session writing a handoff
    doc shell this instead of composing a path, so the configured location is
    the one door.
    """
    import datetime as _dt

    from fno._flag_aliases import merge_deprecated_alias
    from fno.harness_identity import canonical_handle
    from fno.paths import handoffs_dir

    session_id = merge_deprecated_alias(
        session_id, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    if not session_id:
        raise typer.BadParameter("a session id is required (--session-id)")
    key = slug or canonical_handle(session_id)
    filename = f"{_dt.datetime.now().strftime('%Y%m%d')}-{key}.md"
    typer.echo(filename if name_only else str(handoffs_dir() / filename))
