"""``fno doctor reclaim`` (x-7ca7): thin wrapper over ``fno-agents reclaim`` (the lanes live in Rust)."""
from __future__ import annotations

import subprocess
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.rust_binary import resolve_binary


def reclaim_command(
    apply: bool = typer.Option(False, "--apply", help="Remove what the lanes found."),
    verbose: bool = typer.Option(False, "-v", "--verbose", help="List every path."),
) -> None:
    """Reclaim disk bloat: plugin-cache build copies, leaked test HOMEs, stale scratch."""
    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno doctor reclaim: the fno-agents binary was not found. "
            "Reinstall fno, run `fno doctor update --rust`, or set FNO_AGENTS_BIN.",
            err=True,
        )
        raise typer.Exit(code=2)
    argv = [str(binary), "reclaim"]
    argv += ["--apply"] if apply else []
    argv += ["-v"] if verbose else []
    result = subprocess.run(argv, check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
