"""Passthrough for the native ``fno agents transcript`` transfer verbs.

The bundle transfer is Rust (the fno crate's transcript_transfer module);
this shim resolves the native binary and execs it verbatim, the same
shape as the history shim beside it.
"""

from __future__ import annotations

import os
import typer

from fno.events.store_client import resolve_native_bin


def transcript_command(
    args: list[str] = typer.Argument(..., help="send <session-id> [--file <path>] | receive <code|path>"),
) -> None:
    """Move a session bundle to another computer: a pairing code, or a file."""
    argv = ["fno", "agents", "transcript", *args]
    os.execv(resolve_native_bin(), argv)
