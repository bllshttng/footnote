"""``fno doctor bash-census`` - thin wrapper over the `fno-agents bash-census`
Rust verb (x-997a).

Folds this project's Claude transcripts into a Bash-call shape: compound/
leading-cd/heredoc shares, top command heads, top ``fno`` verbs. ``--allow``
turns the top verbs into ``Bash(fno <verb>:*)`` lines for
``/fewer-permission-prompts``. The read itself lives in the Rust binary
(`crates/fno-agents/src/bash_census.rs`); this wrapper only resolves it and
forwards flags, mirroring `cli/src/fno/phase/cli.py`'s kill-check forwarder.
"""
from __future__ import annotations

import subprocess
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.rust_binary import resolve_binary


def bash_census_command(
    days: int = typer.Option(
        21, "--days", help="Window size in days (0 = every transcript on disk)."
    ),
    allow: bool = typer.Option(
        False,
        "--allow",
        help="Print `Bash(fno <verb>:*)` lines for the top verbs, ready to paste "
        "into permissions.allow.",
    ),
    json_output: bool = typer.Option(
        False, "--json", help="Emit the report as one JSON object."
    ),
    cwd: Optional[str] = typer.Option(
        None,
        "--cwd",
        help="Project whose Claude transcripts to read. Defaults to the current directory.",
    ),
) -> None:
    """Fold this project's Bash tool calls into compound/cd/heredoc shares and
    top command/verb tables. Exit 3 when the window holds no Bash calls."""
    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno doctor bash-census: the fno-agents binary was not found. It "
            "ships in the `pip install fno` wheel and with the plugin; "
            "reinstall fno or run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=2)

    argv = [str(binary), "bash-census", "--days", str(days)]
    if allow:
        argv.append("--allow")
    if json_output:
        argv.append("--json")
    if cwd is not None:
        argv.extend(["--cwd", cwd])

    result = subprocess.run(argv, check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
