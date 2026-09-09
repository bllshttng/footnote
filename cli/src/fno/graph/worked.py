"""The positive live-worker authority behind ``fno backlog worked``."""
from __future__ import annotations

import json

import typer


def cmd_worked(
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON."),
) -> None:
    """Show nodes with positively identified live workers."""
    from fno.graph.statuses import live_worked_node_ids
    from fno.graph.store import read_graph_strict
    from fno.paths import graph_json

    try:
        entries = read_graph_strict(graph_json())
        entry_by_id = {entry.get("id"): entry for entry in entries}
        worked = live_worked_node_ids(strict=True, entries=entries)
    except Exception as exc:  # noqa: BLE001 - the authority must refuse loudly
        typer.echo(f"Error: worked authority unavailable: {exc}", err=True)
        raise typer.Exit(code=1) from exc

    rows: list[dict] = []
    for node_id, workers in worked.items():
        entry = entry_by_id.get(node_id) or {}
        phases = []
        for session in entry.get("sessions") or []:
            phase = session.get("phase") if isinstance(session, dict) else None
            if isinstance(phase, str) and phase not in phases:
                phases.append(phase)
        rows.append(
            {
                "id": node_id,
                "status": entry.get("status") or "unknown",
                "workers": workers,
                "phases": phases,
            }
        )

    if json_output:
        typer.echo(json.dumps(rows))
        return
    for row in rows:
        typer.echo(f"{row['id']}  {row['status']}  {', '.join(row['workers'])}")
