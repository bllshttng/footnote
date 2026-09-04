"""``fno doctor bash-census`` (x-997a): thin wrapper over `fno-agents
bash-census`. The fold lives in Rust; this resolves the binary and forwards flags."""
from __future__ import annotations

import subprocess
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.rust_binary import resolve_binary


def bash_census_command(
    days: int = typer.Option(21, "--days", help="Window size in days (0 = every transcript)."),
    allow: bool = typer.Option(False, "--allow", help="Print Bash(fno <verb>:*) allow lines."),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the report as one JSON object."
    ),
    cwd: Optional[str] = typer.Option(None, "--cwd", help="Project to read. Default: cwd."),
) -> None:
    """Bash-call compound/cd/heredoc shares and top command/verb tables.
    Exit 3 when the window holds no Bash calls."""
    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno doctor bash-census: the fno-agents binary was not found. "
            "Reinstall fno, run `fno doctor update --rust`, or set FNO_AGENTS_BIN.",
            err=True,
        )
        raise typer.Exit(code=2)

    argv = [str(binary), "bash-census", "--days", str(days)]
    argv += ["--allow"] if allow else []
    argv += ["--json"] if json_output else []
    argv += ["--cwd", cwd] if cwd is not None else []
    result = subprocess.run(argv, check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
