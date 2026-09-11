"""One seam for "long text from an argument or a file".

Every verb that takes prose positionally gains the same ``--*-file`` twin
through :func:`read_text_arg` (the ``gate_reads.py`` precedent for the
file-budget gate).
"""
from __future__ import annotations

import sys
from pathlib import Path
from typing import Optional, Union

import typer


def read_text_arg(
    inline: Optional[str],
    path: Optional[Union[str, Path]],
    *,
    what: str = "text",
) -> Optional[str]:
    """The text from whichever form was given; None when neither. Refuses both
    at once, reads ``-`` as stdin, refuses an unreadable file."""
    if inline is not None and path is not None:
        typer.echo(f"error: provide {what} once - inline or as a file, not both", err=True)
        raise typer.Exit(code=1)
    if path is None:
        return inline
    if str(path) == "-":
        return sys.stdin.read()
    try:
        return Path(path).read_text(encoding="utf-8")
    except OSError as exc:
        typer.echo(f"error: cannot read {path}: {exc}", err=True)
        raise typer.Exit(code=1) from exc
