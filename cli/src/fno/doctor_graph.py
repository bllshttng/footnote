from __future__ import annotations

import subprocess
from datetime import datetime, timezone
from pathlib import Path
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


# The flip (task 10.1): soak via the keeper's backend_gate op; tree gates via check-graph-flip-gates.sh.


def _gate_gaps(client, repo_root: Path) -> list[str]:
    gaps = list(client.request("backend_gate", {}).get("gaps") or [])
    try:
        proc = subprocess.run(
            ["bash", str(repo_root / "scripts/ci/check-graph-flip-gates.sh")],
            capture_output=True, text=True, timeout=2400,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        gaps.append(f"flip gates could not run: {exc}")
    else:
        gaps.extend(
            line[len("flip-gate: FAIL: ") :]
            for line in (proc.stdout + proc.stderr).splitlines()
            if line.startswith("flip-gate: FAIL: ")
        )
        if proc.returncode not in (0, 1):
            gaps.append("flip gates could not run (see scripts/ci/check-graph-flip-gates.sh)")
    return gaps


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

    client = _client_for(paths.graph_json())
    current = str(client.request("backend_status", {}).get("backend"))
    if target == current:
        typer.echo(f"backend={current} already; nothing to flip")
        return
    if target == "sqlite":
        gaps = _gate_gaps(client, Path(__file__).resolve().parents[3])
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
    typer.echo(f"backend={target} keepers={_keepers()}")


@graph_app.command("backend")
def graph_backend(
    target: "str | None" = typer.Argument(None, help="sqlite, json, or status."),
) -> None:
    if target == "status":
        from fno import paths
        from fno.graph.store import _client_for

        state = _client_for(paths.graph_json()).request("backend_status", {})
        since = (
            datetime.fromtimestamp(int(state["since_ms"]) / 1000, tz=timezone.utc)
            if state.get("since_ms")
            else None
        )
        since_text = since.strftime("%Y-%m-%d") if since else "never"
        days = (datetime.now(timezone.utc) - since).days if since else 0
        typer.echo(
            f"backend={state.get('backend')} since={since_text} days={days} keepers={_keepers()}"
        )
        return
    if target not in ("sqlite", "json"):
        raise typer.BadParameter("backend takes 'sqlite', 'json', or 'status'")
    _flip(target)
