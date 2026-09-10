from __future__ import annotations

import typer

graph_app = typer.Typer(help="Inspect or export the durable graph store.")

def export_health() -> dict:
    from fno import paths
    from fno.graph.store import _client_for
    try:
        return _client_for(paths.graph_json()).request("export_status", {})
    except Exception as exc:  # noqa: BLE001 - doctor reports without crashing
        return {"backend": "unknown", "stale": False, "error": str(exc)}

@graph_app.command("export")
def export_graph(now: bool = typer.Option(False, "--now", help="Wait for a fresh JSON export.")) -> None:
    if not now:
        raise typer.BadParameter("export currently requires --now")
    from fno import paths
    from fno.graph.store import _client_for
    result = _client_for(paths.graph_json()).request("export_now", {})
    typer.echo(f"graph export: {result['path']} at {result['version']}")
