"""``fno agents king ledger`` - the reign ledger page.

Identity, the court adjudication, and the paths stay in Python; the page
assembly is the native ``reign-ledger`` verb (the king-history split), so
the Python-tree ratchet holds. Contract: docs/architecture/reign.md.
"""
from __future__ import annotations

import json
import os
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional


def build_ledger_data(rows=None, *, fold_fn=None) -> dict:
    """gather_court plus the native scope fold; the ledger's whole input."""
    from fno.agents.court import fold_scope_nodes, gather_court

    court = gather_court(rows)
    crowns = court.get("crowns")
    if crowns:
        (fold_fn or fold_scope_nodes)(crowns)
    return court


def default_ledger_path() -> Path:
    """``<state_dir>/reign.html``, the sibling of graph.html."""
    try:
        from fno import paths as _paths

        return _paths.state_dir() / "reign.html"
    except Exception:
        return Path.home() / ".fno" / "reign.html"


def write_ledger(court: dict, path: Optional[Path] = None) -> Path:
    """Relay the court to the native renderer; returns the path written."""
    from fno.paths import graph_json
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise RuntimeError(
            "the fno-agents binary was not found: the reign ledger page is "
            "rendered by the native reign-ledger verb"
        )
    fd, court_file = tempfile.mkstemp(suffix=".json")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(court, handle)
        argv = [
            str(binary),
            "reign-ledger",
            "--court-json",
            court_file,
            "--graph",
            str(graph_json()),
            "--generated",
            datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "--out",
            str(path),
        ]
        proc = subprocess.run(argv, capture_output=True, text=True, check=False, timeout=60)
    finally:
        try:
            os.unlink(court_file)
        except OSError:
            pass
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip() or f"reign-ledger exited {proc.returncode}")
    return path
