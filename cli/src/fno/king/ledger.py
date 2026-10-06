"""``fno agents king ledger`` - the reign ledger page.

Identity, the court adjudication, and the paths stay in Python; the page
assembly is the native ``reign-ledger`` verb (the king-history split), so
the Python-tree ratchet holds. Contract: docs/architecture/lead.md.
"""
from __future__ import annotations

import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional


def build_ledger_data(rows=None, *, fold_fn=None) -> dict:
    """gather_court plus the native scope fold; the ledger's whole input."""
    from fno.agents.court import fold_scope_nodes, gather_court

    court = gather_court(rows)
    crowns = court.get("crowns")
    if crowns:
        (fold_fn or fold_scope_nodes)(crowns)
    return court


def default_ledger_path() -> Path:
    """``<state_dir>/pages/reign.html``, beside the rendered graph pages."""
    from fno.graph._constants import _state_dir

    return _state_dir() / "pages" / "reign.html"


def write_ledger(court: dict, path: Optional[Path] = None) -> Path:
    """Relay the court to the native renderer; returns the path written."""
    from fno.paths import graph_json
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise RuntimeError(
            "the fno-agents binary was not found: the reign ledger page is "
            "rendered by the native lead-rundown verb"
        )
    out = Path(path) if path is not None else default_ledger_path()
    argv = [
        str(binary),
        "lead-rundown",
        "--court-json",
        "-",
        "--graph",
        str(graph_json()),
        "--generated",
        datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "--out",
        str(out),
    ]
    # Load-scaled bound: 60s idle, capped at half the arm's 300s beat, so a
    # starved machine's page render is not killed into a permanent error.
    try:
        load = os.getloadavg()[0] / (os.cpu_count() or 1)
    except (AttributeError, OSError):
        load = 0.0
    proc = subprocess.run(
        argv, input=json.dumps(court), capture_output=True, text=True, check=False,
        timeout=min(150.0, 60.0 * max(1.0, load / 2.0)),
    )
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip() or f"lead-rundown exited {proc.returncode}")
    return out
