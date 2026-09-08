"""Board-collection helpers for the king board's blocked_child queue (x-3ecf).

Registered on the agents app by importing this module from cli.py (after
agents_app exists), so the file-budget gate keeps cli.py shrinking - the same
pattern transcript_reads.py uses.

The Rust queue reads the distress journal (events.jsonl) and the claim/graph
sources it already has in-process; this file answers the one question it
cannot answer without duplicating a Python-owned reader: has mail addressed
to a session landed since its blocked row, and what does the fleet watchdog
currently say about that session. Both come from ONE subprocess call so N
blocked rows cost one spawn, not N.
"""

from __future__ import annotations

import json as _json
from typing import Any

import typer

from fno.agents.cli import agents_app


@agents_app.command("distress-answered", hidden=True)
def cmd_distress_answered(
    pairs: str = typer.Option(
        ...,
        "--pairs",
        help='JSON array of {"session": <id>, "after": <RFC3339 ts>} objects.',
    ),
) -> None:
    """Print ``{"<session>": {"answered": bool, "watchdog_verdict": str|null}}``.

    ``answered`` is true when the bus log carries mail addressed to that
    session (``to == session``) with ``ts`` after the paired ``after``
    timestamp - the mail-answered signal AC3-EDGE names. ``watchdog_verdict``
    is this session's current word from :func:`fno.agents.watchdog.session_verdict`
    (``None`` on a read failure or an unknown session); informational only,
    never a gate - the board's inclusion decision goes by the three named
    signals (mail, claim release, node closing), not this one.
    """
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

    # Keep the OLDEST `after` per session: a session can carry more than one
    # open blocked row and the board shows the oldest, so that is the cutoff
    # a later reply must clear.
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
