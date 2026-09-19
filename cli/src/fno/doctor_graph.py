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


# The flip: soak gaps come from the keeper's backend_gate op, the parity
# negative control runs in-process, and the tree checks (reader census,
# writer ratchet, table ownership) are CI's job on every PR and main push.


def _gate_gaps(client) -> list[str]:
    """Keeper soak gaps plus the in-process parity negative control. The
    tree checks (reader census, writer ratchet, table ownership) belong to
    CI: guards.yml runs them on every pull request and every push to main."""
    gaps = list(client.request("backend_gate", {}).get("gaps") or [])
    from fno.graph.parity import negative_control

    if negative_control() != 0:
        gaps.append("negative control failed")
    return gaps


def _keeper_gaps(client) -> list[str]:
    """The soak clock alone, for the read-only status watch: no copy, no
    temp keeper, no negative control."""
    return list(client.request("backend_gate", {}).get("gaps") or [])


def _flip(target: str) -> None:
    from fno import paths
    from fno.graph.store import _client_for

    client = _client_for(paths.graph_json())
    current = str(client.request("backend_status", {}).get("backend"))
    if target == current:
        typer.echo(f"backend={current} already; nothing to flip")
        return
    if target == "sqlite":
        gaps = _gate_gaps(client)
        if gaps:
            for gap in gaps:
                typer.echo(f"graph backend: refused: {gap}", err=True)
            raise typer.Exit(1)
    else:  # Rollback exports FIRST: sqlite still owns the rows until the flip.
        try:
            client.request("export_now", {})
        except Exception as exc:  # noqa: BLE001 - a failed export refuses, never crashes
            typer.echo(f"graph backend: refused: export before flip failed: {exc}", err=True)
            raise typer.Exit(1) from exc
    client.request("set_backend", {"backend": target})
    try:
        from fno.config.writer import set_config_value
        set_config_value("graph.read_source", target, scope="global")
    except Exception as exc:  # noqa: BLE001 - keeper flipped; name the remedy
        typer.echo(f"graph backend: keeper flipped but graph.read_source not written ({exc}); run `fno config set graph.read_source {target}`", err=True)
    typer.echo(f"backend={target}")


def _illegal_canonical_keepers(client, backend) -> "list[tuple[int, int]]":
    """Store keepers on the canonical graph, read through the store's own
    `keeper_scan` op; legality rides the backend this verb already fetched
    plus the configured read_source. A hit is a stale binary or a
    hand-spawn; the status verb refuses and names the collector."""
    from fno.agents.keeper_lane import graph_read_source

    if graph_read_source() != "sqlite" or backend != "sqlite":
        return []
    return [
        (int(row["pid"]), int(row.get("rss_kb") or 0))
        for row in client.request("keeper_scan", {}).get("keepers") or []
    ]


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
        illegal = _illegal_canonical_keepers(client, state.get("backend"))
        for pid, rss_kb in illegal:
            gb = rss_kb / (1024 * 1024)
            typer.echo(
                f"refused: resident keeper pid {pid} holds the canonical graph "
                f"({gb:.2f} GB RSS) while backend=sqlite; collect it: "
                f"fno agents watchdog --only keeper --apply-all",
                err=True,
            )
        if illegal:
            raise typer.Exit(1)
        typer.echo("keepers: fno agents watchdog --only keeper")
        gaps = _keeper_gaps(client)
        if gaps:
            for gap in gaps:
                typer.echo(f"gate: {gap}")
        else:
            typer.echo("gate: soak clean")
        return
    if target not in ("sqlite", "json"):
        raise typer.BadParameter("backend takes 'sqlite', 'json', or 'status'")
    _flip(target)
