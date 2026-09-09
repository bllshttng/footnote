"""Watchdog-verdict lookup for the king board's blocked_child queue
(x-3ecf Change 2). Registered via import from cli.py, matching
transcript_reads.py, so cli.py itself stays net 0."""

from __future__ import annotations

import json as _json
from typing import Any

import typer

from fno.agents.cli import agents_app


@agents_app.command("distress-verdicts", hidden=True)
def cmd_distress_verdicts(
    sessions: str = typer.Option(..., "--sessions", help="JSON array of session ids."),
) -> None:
    """Print ``{"<session>": "<verdict>"|null}``."""
    try:
        ids = _json.loads(sessions)
    except (TypeError, ValueError):
        typer.echo("fno agents distress-verdicts: --sessions must be JSON", err=True)
        raise typer.Exit(code=2)
    if not isinstance(ids, list):
        typer.echo("fno agents distress-verdicts: --sessions must be a JSON array", err=True)
        raise typer.Exit(code=2)

    verdict_by_session: dict[str, Any] = {}
    try:
        from fno.agents.watchdog import run_sweep

        payload, _rows = run_sweep()
        verdict_by_session = {
            v.get("row_id"): v.get("verdict") for v in payload.get("verdicts", [])
        }
    except Exception:  # noqa: BLE001 - enrichment only, never fatal
        pass

    out = {s: verdict_by_session.get(s) for s in ids if isinstance(s, str)}
    typer.echo(_json.dumps(out))
