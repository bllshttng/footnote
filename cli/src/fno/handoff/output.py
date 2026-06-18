"""Shared helpers for CLI subcommands that emit JSON or prose output.

All subcommands honor ``--json`` for structured stdout (spec requirement).
The helpers here let a subtyper or leaf command declare ``--json`` without
each call site re-implementing the ctx.obj merge dance.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

import typer


def merge_json_flag(ctx: typer.Context, json_output: bool) -> None:
    """Merge a leaf/subtyper --json flag into ctx.obj.

    Parent callbacks, subtyper callbacks, and leaf commands can each
    declare their own --json option. Whichever is set wins; the others
    inherit via ctx.obj so downstream helpers (:func:`json_mode`,
    :func:`emit`) read a single source of truth.
    """
    ctx.ensure_object(dict)
    if json_output:
        ctx.obj["json"] = True


def json_mode(ctx: typer.Context) -> bool:
    """Return True when the current invocation requested JSON output."""
    return bool(ctx.obj and ctx.obj.get("json", False))


def emit(ctx: typer.Context, data: Any) -> None:
    """Write ``data`` to stdout as JSON (when --json) or prose."""
    if json_mode(ctx):
        typer.echo(json.dumps(data, default=str))
        return
    if isinstance(data, dict):
        for key, value in data.items():
            typer.echo(f"{key}: {value}")
    elif isinstance(data, list):
        typer.echo(json.dumps(data, default=str))
    else:
        typer.echo("" if data is None else str(data))


def emit_error(ctx: typer.Context, message: str) -> None:
    """Emit an error message on the correct stream for the current mode.

    - ``--json`` mode writes ``{"ok": false, "error": message}`` to stdout
      so a pipe consumer can parse it.
    - Prose mode writes to stderr (stdout stays reserved for data).
    """
    if json_mode(ctx):
        typer.echo(json.dumps({"ok": False, "error": message}))
    else:
        typer.echo(f"error: {message}", file=sys.stderr)


def write_output_file(path: Path, content: str) -> dict[str, Any]:
    """Write ``content`` to ``path``, creating parents. Return summary dict.

    Used by subcommands that support ``--output``. When the caller asks
    for a file, stdout gets only a small summary JSON (``{"ok": true,
    "output_path": "...", "bytes_written": N}``) so pipelines can parse
    it without scanning a huge report body.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return {
        "ok": True,
        "output_path": str(path),
        "bytes_written": len(content.encode("utf-8")),
    }
