"""Batched mail-answered + watchdog-verdict lookup for the king board's
blocked_child queue (x-3ecf). Registered via import from cli.py, same
pattern as transcript_reads.py, so cli.py itself stays net 0."""

from __future__ import annotations

import json as _json
from typing import Any

import typer

from fno.agents.cli import agents_app


@agents_app.command("distress-answered", hidden=True)
def cmd_distress_answered(
    pairs: str = typer.Option(
        ..., "--pairs", help='JSON array of {"session": <id>, "after": <RFC3339 ts>} objects.'
    ),
) -> None:
    """Print ``{"<session>": {"answered": bool, "watchdog_verdict": str|null}}``.
    ``answered``: mail to that session landed after ``after`` (AC3-EDGE).
    ``watchdog_verdict`` is informational only, never a gate."""
    from fno.agents import watchdog as wd
    from fno.bus.log import iter_messages

    try:
        requested = _json.loads(pairs)
    except (TypeError, ValueError):
        typer.echo("fno agents distress-answered: --pairs must be JSON", err=True)
        raise typer.Exit(code=2)
    if not isinstance(requested, list):
        typer.echo("fno agents distress-answered: --pairs must be a JSON array", err=True)
        raise typer.Exit(code=2)

    after_by_session: dict[str, str] = {}
    for item in requested:
        session = item.get("session") if isinstance(item, dict) else None
        after = item.get("after") if isinstance(item, dict) else None
        if isinstance(session, str) and session and isinstance(after, str) and after:
            if session not in after_by_session or after < after_by_session[session]:
                after_by_session[session] = after

    answered = {session: False for session in after_by_session}
    for env in iter_messages(warn=False):
        cutoff = after_by_session.get(env.to)
        if cutoff is not None and env.ts > cutoff:
            answered[env.to] = True

    out: dict[str, Any] = {}
    for session in after_by_session:
        try:
            verdict = wd.session_verdict(session)
        except Exception:  # noqa: BLE001 - enrichment only, never fatal
            verdict = None
        out[session] = {"answered": answered[session], "watchdog_verdict": verdict}
    typer.echo(_json.dumps(out))
