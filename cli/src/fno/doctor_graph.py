from __future__ import annotations

from datetime import datetime, timezone
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


# The flip: sqlite is the only store, so the verb stamps the meta and the
# tree checks (reader census, writer ratchet, table ownership) are CI's job
# on every PR and main push.


def _keepers() -> str:
    from fno import paths
    from fno.graph.store import _Keeper

    backends = []
    for sock in sorted(paths.state_dir().rglob("*.store.sock")):
        try:
            backends.append(str(_Keeper(sock).identify().get("store_backend", "unknown")))
        except Exception as exc:  # noqa: BLE001 - report, never crash
            backends.append(f"unreachable: {exc}")
    return "{" + ", ".join(f"'{b}'" for b in backends) + "}"


def _flip(target: str) -> None:
    from fno import paths
    from fno.graph.store import _client_for

    if target != "sqlite":
        typer.echo(
            "graph backend: refused: the json backend is deleted; sqlite is the only store",
            err=True,
        )
        raise typer.Exit(1)
    client = _client_for(paths.graph_json())
    # Idempotent by keeper contract: a re-run keeps the original since
    # stamp, so the first run on a fresh store is what starts the clock.
    client.request("set_backend", {"backend": target})
    typer.echo(f"backend={target} keepers={_keepers()}")


@graph_app.command("backend")
def graph_backend(
    target: "str | None" = typer.Argument(None, help="sqlite, json, or status."),
) -> None:
    if target == "status":
        from fno import paths
        from fno.graph.store import _client_for

        client = _client_for(paths.graph_json())
        state = client.request("backend_status", {})
        since = (
            datetime.fromtimestamp(int(state["since_ms"]) / 1000, tz=timezone.utc)
            if state.get("since_ms")
            else None
        )
        since_text = since.strftime("%Y-%m-%d") if since else "never"
        days = (datetime.now(timezone.utc) - since).days if since else 0
        typer.echo(
            f"backend={state.get('backend')} since={since_text} days={days}"
        )
        return
    if target not in ("sqlite", "json"):
        raise typer.BadParameter("backend takes 'sqlite', 'json', or 'status'")
    _flip(target)
