"""`fno backlog get`, several ids: forward to `fno-agents graph-get` (x-997a). Split out of graph/cli.py (over-budget)."""
from __future__ import annotations

import subprocess
from typing import List

import typer


def resolve_or_dispatch(ids: List[str], *, field: object, grouped: bool, strict: bool) -> str:
    """A single id: return it. Several: dispatch the batch read, never return."""
    if len(ids) <= 1:
        return ids[0]
    if field or grouped or strict:
        typer.echo(
            "fno backlog get: --field/--grouped/--strict take exactly one id; "
            "pass one id at a time for those, or drop them for a plain batch read.",
            err=True,
        )
        raise typer.Exit(code=2)
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno backlog get: the fno-agents binary was not found, and a batch "
            "read of several ids needs it. Reinstall fno, run `fno doctor update "
            "--rust`, or set FNO_AGENTS_BIN. Pass one id at a time to use the "
            "all-Python path instead.",
            err=True,
        )
        raise typer.Exit(code=2)
    result = subprocess.run([str(binary), "graph-get", *ids, "--json"], check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
