"""`fno backlog get`, several ids: forward to the Rust `graph-get` batch verb
rather than reading graph.json once per id (x-997a census deliverable 4).

Mirrors `cli/src/fno/phase/cli.py`'s kill-check forwarder: resolve the
bundled binary, print its own not-found message with the reinstall remedy on
a missing binary, else run it and propagate its exit code and stdout (a JSON
array in argument order) untouched. Split out of `graph/cli.py` (a file over
the project's 5,000-line shrink-only budget) so the new code lands in a
module named by the question it answers, per `scripts/ci/check-file-budget.sh`.
"""
from __future__ import annotations

import subprocess
from typing import List

import typer


def resolve_or_dispatch(
    ids: List[str], *, field: object, grouped: bool, strict: bool
) -> str:
    """A single id: return it, so `cmd_get` falls through to its existing
    all-Python path untouched. Several ids: dispatch the batch read and never
    return (`typer.Exit` from `_forward`, or from the flag refusal below)."""
    if len(ids) <= 1:
        return ids[0]
    if field or grouped or strict:
        typer.echo(
            "fno backlog get: --field/--grouped/--strict take exactly one id; "
            "pass one id at a time for those, or drop them for a plain batch read.",
            err=True,
        )
        raise typer.Exit(code=2)
    _forward(ids)
    raise AssertionError("unreachable: _forward always raises typer.Exit")


def _forward(ids: List[str]) -> None:
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno backlog get: the fno-agents binary was not found, and a batch "
            "read of several ids needs it. It ships in the `pip install fno` "
            "wheel and with the plugin; reinstall fno or run `fno doctor update "
            "--rust`, or set FNO_AGENTS_BIN to its path. Pass one id at a time "
            "to use the all-Python path instead.",
            err=True,
        )
        raise typer.Exit(code=2)
    result = subprocess.run([str(binary), "graph-get", *ids, "--json"], check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
