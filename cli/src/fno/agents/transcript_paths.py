import json
import sys
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


def cmd_transcript_paths():
    """One JSON map of id -> transcript path; the batch rides stdin (flagless)."""
    payload = json.load(sys.stdin)
    root = Path(p) if (p := payload.get("projects_root")) else None
    typer.echo(json.dumps(resolve_paths(payload.get("ids") or [], projects_root=root)))
