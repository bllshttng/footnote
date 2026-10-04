"""CLI surface for path introspection: fno config paths shell-stub / verify.

emit-shell and handoff answer natively (crates/fno-agents/src/paths_cli.rs);
this front keeps a forwarding leaf for each so `fno-py` and the test harnesses
that wrap it reach the native lane through the one Rust classify. The rest of
this group still serves shell-stub and verify until their child nodes port
(one verb per PR, d-450caaeb).
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


def _forward_native(verb: str, ctx: typer.Context) -> None:
    """Exec the Rust front with the same argv, propagating its exit code.

    The Rust front owns the classify (crates/fno/src/paths_route.rs) and the
    worker_binary resolution; this front never re-states the native verb list.
    """
    from fno.cli import _run_rust_front

    _run_rust_front(["config", "paths", verb, *ctx.args])


@app.command(
    "handoff",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    hidden=True,
)
def handoff(ctx: typer.Context) -> None:
    """Forward to the native handoff verb (crates/fno-agents paths_cli.rs)."""
    _forward_native("handoff", ctx)


@app.command(name="shell-stub")
def shell_stub() -> None:
    """Generate a fresh paths.sh from current settings and print its path.

    Bash callers use: source "$(fno config paths shell-stub)".

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
            "Generate it with: fno config paths emit-shell",
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
            f"  fno config paths emit-shell",
            err=True,
        )
        raise typer.Exit(code=1)
