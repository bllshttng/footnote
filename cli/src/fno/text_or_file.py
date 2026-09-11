"""One seam for "long text from an argument or a file".

Every verb that takes prose positionally gains the same ``--*-file`` twin
through :func:`read_text_arg`, so read-or-refuse lives in one place (the
``gate_reads.py`` precedent: registered here so the file-budget gate keeps
the calling CLIs shrinking).
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
    """The text, from whichever of the two forms was given; None when neither.

    Refuses both at once, treats ``-`` as stdin, and refuses an unreadable
    file rather than tracebacking. ``what`` names the argument in refusals.
    """
    if inline is not None and path is not None:
        typer.echo(
            f"error: provide {what} once - inline or as a file, not both", err=True
        )
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
