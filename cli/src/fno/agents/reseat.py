<<<<<<< HEAD
"""Re-seat a live pane worker into a portal: the registry half of the v70 move.
=======
"""Re-seat a live pane worker into a portal: the registry half of the v70 move.
>>>>>>> 717fade04 (fix(proto): take wire version 70; 1499 (SquadReload) lands at 69 first)

Drives ``fno mux thread reseat <pane>`` (the server moves the topology keeping
the PTY), then clears the row's ``mux`` ref on the receipt - the server reads
the registry, never writes it. Both halves are idempotent, so a re-run after a
half-completed move converges. After the move the row is a thread row: not
rebuilt by restore, and ``agents rm`` drops it without killing the pane.
"""
from __future__ import annotations

import json
import os
import subprocess
from typing import Callable, Optional

import typer

from fno.agents.registry import AgentResolutionError, resolve_agent, update_registry


class ReseatError(RuntimeError):
    """A refused re-seat; the message names the refusal."""


def run_reseat(
    token: str,
    *,
    portal: Optional[int] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    registry_path=None,
) -> dict[str, object]:
    """Move one pane-hosted row's live pane into a portal and flip its ref."""
    try:
        entry = resolve_agent(token, path=registry_path).entry
    except AgentResolutionError as exc:
        raise ReseatError(f"unknown_row: {exc}") from exc
    mux = entry.mux
    if not isinstance(mux, dict) or not mux.get("session") or not mux.get("pane_id"):
        raise ReseatError("not_pane_hosted: only a pane-hosted row can re-seat")
    session, pane_id = str(mux["session"]), int(mux["pane_id"])
    args = ["mux", "thread", "reseat", "--session", session, str(pane_id)]
    if portal is not None:
        args += ["--portal", str(portal)]
    try:
        proc = runner(
            [os.environ.get("FNO_BIN") or "fno", *args],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise ReseatError(f"mux_unreachable: {exc}") from exc
    stderr = (proc.stderr or "").strip()
    if proc.returncode != 0:
        code = "mux_unreachable" if "no live mux server" in stderr else "server_refused"
        raise ReseatError(f"{code}: {stderr or proc.stdout.strip()}")
    landing = (proc.stdout or "").strip()

    def flip(entries: list) -> list:
        for row in entries:
            if row.name == entry.name:
                row.mux = None
        return entries

    try:
        update_registry(flip, path=registry_path)
    except OSError as exc:
        raise ReseatError(
            f"registry_flip_failed: the pane moved ({landing}) but the ref was "
            f"not cleared; re-run fno agents reseat {token} ({exc})"
        ) from exc
    return {"status": "reseated", "row": entry.name, "pane_id": pane_id, "landing": landing}


def reseat_command(
    token: str = typer.Argument(..., help="Pane-hosted worker name or session id."),
    portal: Optional[int] = typer.Option(None, "--portal", help="Portal index; default next free."),
    json_out: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Re-seat a live pane worker into a portal, keeping its PTY.

    The row becomes a thread row: not rebuilt by restore, and rm drops it
    without killing the pane. The work survives; the geometry does not.
    """
    try:
        receipt = run_reseat(token, portal=portal)
    except ReseatError as exc:
        typer.echo(f"agents reseat: {exc}", err=True)
        raise typer.Exit(code=1)
    typer.echo(json.dumps(receipt) if json_out else receipt.get("landing") or json.dumps(receipt, sort_keys=True))
