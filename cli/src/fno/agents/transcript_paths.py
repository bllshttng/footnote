import json
from pathlib import Path

import typer

from fno.provenance.resolver import resolve_transcript, transcript_listing


def resolve_paths(ids, projects_root=None):
    """Every id answered the way ``fno agents peek`` resolves: THE resolver, one shared listing."""
    out = {sid: None for sid in ids}
    with transcript_listing(projects_root):
        for sid in ids:
            rt = resolve_transcript("claude", sid, "/", projects_root=projects_root)
            if rt.resolved and rt.transcript_path:
                out[sid] = rt.transcript_path
    return out


def cmd_transcript_paths(ids=typer.Option(..., "--ids"), projects_root: Path | None = typer.Option(None, "--projects-root")):
    typer.echo(json.dumps(resolve_paths([v for v in ids.split(",") if v.strip()], projects_root=projects_root)))
