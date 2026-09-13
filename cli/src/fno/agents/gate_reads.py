"""``fno agents gate-status``: the gate's read-only verdict; registered here so the file-budget gate keeps ``agents/cli.py`` shrinking."""
from __future__ import annotations

import json

from fno.agents.cli import agents_app


@agents_app.command("gate-status")
def cmd_gate_status() -> None:
    """Print the spawn gate's read-only capacity verdict as JSON."""
    from fno.agents.spawn_gate import probe_capacity

    print(json.dumps(probe_capacity()))
