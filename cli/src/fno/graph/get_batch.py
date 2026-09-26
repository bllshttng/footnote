"""`fno backlog get`, several ids and the native exec shim. Split out of graph/cli.py (over-budget)."""
from __future__ import annotations

import subprocess
from typing import List

import typer


def resolve_or_dispatch(ids: List[str], *, field: object, grouped: bool, strict: bool) -> str:
    if len(ids) > 1:
        _dispatch(ids, field=field, grouped=grouped, strict=strict)
    return ids[0]


def exec_graph(ids: List[str]) -> None:
    _exec(["backlog", "get", *ids])


def _dispatch(ids: List[str], *, field: object, grouped: bool, strict: bool) -> None:
    if field or grouped or strict:
        typer.echo(
            "fno backlog get: --field/--grouped/--strict take exactly one id; "
            "pass one id at a time for those, or drop them for a plain batch read.", err=True,
        )
        raise typer.Exit(code=2)
    _exec(["backlog", "get", *ids, "--json"])


def _exec(argv: List[str]) -> None:
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno backlog get: the fno-agents binary was not found, and a native read "
            "needs it. Reinstall fno, run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN.", err=True,
        )
        raise typer.Exit(code=2)
    result = subprocess.run([str(binary), *argv], check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
