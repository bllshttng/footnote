"""Path resolver for the native ``fno agents history`` reader."""

from __future__ import annotations

import os
from pathlib import Path

from fno import paths as _paths
from fno.events.store_client import resolve_native_bin
from fno.graph._reconcile import resolve_current_repo_slug


def history_command(arg: str) -> None:
    """Print each matching session's whole story: node, stages, models, switch, events and resume."""
    argv = [
        "fno", "agents", "history", arg,
        "--graph", str(_paths.graph_json()),
        "--ledger", str(_paths.ledger_json()),
        "--events", str(_paths.global_events_json()),
        "--agents-home", str(_paths.agents_home_dir()),
    ]
    slug = resolve_current_repo_slug(str(Path.cwd()))
    if slug:
        argv.extend(["--repo-slug", slug])
    os.execv(resolve_native_bin(), argv)
