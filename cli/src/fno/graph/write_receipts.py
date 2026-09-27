"""Read-back guards for backlog write receipts."""

from __future__ import annotations

from pathlib import Path

import typer


def confirm_created_row(path: Path, node_id: str | None) -> None:
    if node_id is None:
        return
    from fno.graph.store import _readback_row

    row, answered = _readback_row(path, node_id)
    if not answered:
        typer.echo(
            f"Error: filed {node_id} but the read-back could not confirm it "
            "(store read failed); verify before re-filing",
            err=True,
        )
        raise typer.Exit(code=1)
    if not row:
        typer.echo(
            f"Error: filed {node_id} but it does not read back from the "
            "store; the write did not land",
            err=True,
        )
        raise typer.Exit(code=1)


