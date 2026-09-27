"""The batch transcript-path answer the provider-cap actor reads.

The actor walks registry rows in Rust every two minutes, and a Rust
reimplementation of the transcript lookup is the drift that left live
thread workers reading ``transcript-not-found`` while their lane walled:
peek resolves claude transcripts with a store-wide prefix glob
(``fno.provenance.resolver.resolve_transcript``), the actor's Rust walk
exact-matched ``<uuid>.jsonl`` after a sessions-dir hop. This module is
the one answer both read: one child process per sweep, every id resolved
through THE resolver, under one shared store listing.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer

from fno.provenance.resolver import resolve_transcript, transcript_listing


def resolve_paths(
    ids: list[str],
    *,
    projects_root: Optional[Path] = None,
) -> dict[str, Optional[str]]:
    """``{id: transcript path | None}`` for the batch, through THE resolver.

    Every id - a full uuid or an 8-hex thread-row short id - resolves the way
    ``fno agents peek`` resolves (``resolve_transcript``'s store-wide prefix
    glob, cwd passed as ``/`` exactly as discover.py's own batch passes
    ``cwd or "/"`` because the search never scopes by cwd). One
    ``transcript_listing`` scope shares one store walk across the batch.

    An ambiguous prefix answers the first-sorted match rather than refusing:
    this read feeds a cap tail, not an injection address.
    """
    out: dict[str, Optional[str]] = {sid: None for sid in ids}
    with transcript_listing(projects_root):
        for sid in ids:
            rt = resolve_transcript("claude", sid, "/", projects_root=projects_root)
            if rt.resolved and rt.transcript_path:
                out[sid] = rt.transcript_path
    return out


def cmd_transcript_paths(
    ids: str = typer.Option(
        ...,
        "--ids",
        help="Comma-separated session ids (full uuid or 8-hex short id).",
    ),
    projects_root: Optional[Path] = typer.Option(
        None,
        "--projects-root",
        help="Claude projects store root; default reads the ambient store.",
    ),
) -> None:
    """One JSON map of id -> transcript path for the provider-cap actor."""
    parsed = [v for v in (ids or "").split(",") if v.strip()]
    answer = resolve_paths(parsed, projects_root=projects_root)
    typer.echo(json.dumps(answer))
