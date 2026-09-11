"""`fno agents pane-identity`: cross-check mux panes against registry rows.

A hidden verb: operator-facing reads go through `fno agents list`. This
module exists because `agents/cli.py` sits over the line budget, so the
verb moved here, named by the question it answers. The cross-check core is
`fno.agents.reachability.pane_identity_crosscheck`.
"""
from __future__ import annotations

import sys
from typing import Optional

import typer


def cmd_pane_identity(
    server: Optional[str] = typer.Option(
        None,
        "--server",
        help="Mux server to check. Default: the resolved server.",
    ),
    session_id: Optional[str] = typer.Option(
        None,
        "--session-id",
        hidden=True,
        help="Deprecated alias for --server.",
    ),
    session_legacy: Optional[str] = typer.Option(
        None,
        "--session",
        hidden=True,
        help="Deprecated alias for --server.",
    ),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit the same content as JSON."
    ),
) -> None:
    """Cross-check mux panes against registry rows, in both directions.

    Every registry row with a mux ref must resolve to a pane whose fno_id
    matches the row (a stale ref means the pane was re-homed, e.g. by a
    resume); every pane whose argv carries fno's spawn signature must be
    referenced by a row (a miss is an fno worker no fno surface can address).
    The counts compared print on every run, so a zero-mismatch result is a
    reading and not a silence. A mismatch is a READING, never a repair: this
    verb mutates nothing, and it never mints an identity from argv.

    Exit codes: 0 clean, 1 mismatch found, 2 an instrument (mux listing or
    registry) could not be read.
    """
    import json as _json
    import subprocess as _subprocess

    from fno._flag_aliases import merge_deprecated_alias
    from fno.agents.mux_spawn import _run_mux, resolve_mux_session
    from fno.agents.reachability import (
        pane_identity_crosscheck,
        render_pane_identity_crosscheck,
    )
    from fno.agents.registry import load_registry

    session_name = resolve_mux_session(
        merge_deprecated_alias(
            merge_deprecated_alias(
                server,
                session_id,
                canonical_flag="--server",
                legacy_flag="--session-id",
            ),
            session_legacy,
            canonical_flag="--server",
            legacy_flag="--session",
        )
    )
    listing = _run_mux(
        ["mux", "pane", "ls", "--server", session_name, "--json"], _subprocess.run
    )
    if listing.returncode != 0 or not (listing.stdout or "").strip():
        detail = (listing.stderr or "").strip() or "pane ls returned non-zero"
        print(f"pane-identity: mux listing unavailable: {detail}", file=sys.stderr)
        raise typer.Exit(2)
    try:
        panes = _json.loads(listing.stdout)
    except _json.JSONDecodeError as exc:
        print(f"pane-identity: unparseable pane ls JSON: {exc}", file=sys.stderr)
        raise typer.Exit(2)
    if not isinstance(panes, list):
        print("pane-identity: pane ls JSON was not a list", file=sys.stderr)
        raise typer.Exit(2)
    try:
        rows = load_registry()
    except Exception as exc:  # noqa: BLE001 - an unreadable registry is a broken instrument
        print(f"pane-identity: registry unavailable: {exc}", file=sys.stderr)
        raise typer.Exit(2)
    result = pane_identity_crosscheck(panes, rows, session_name)
    if as_json:
        print(_json.dumps(result, indent=2))
    else:
        print(render_pane_identity_crosscheck(result))
    if result["row_mismatches"] or result["pane_mismatches"]:
        raise typer.Exit(1)
