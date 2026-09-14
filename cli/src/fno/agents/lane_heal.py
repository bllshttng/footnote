"""Ask the hidden `fno-agents lane-heal` verb about a row's pane binding.

One Python shim over one Rust verdict verb (x-4a68): the door owns the
decision, this only shells it and parses the one JSON line.
"""
from __future__ import annotations

import json
import subprocess
from typing import Optional

from fno import rust_binary

_MAIL_INJECT_TIMEOUT_S = 30


def lane_heal(session_id: str) -> tuple[str, Optional[str], Optional[dict]]:
    """Returns ``(verdict, reason, pane)`` where verdict is the Rust vocabulary
    (``no-mux-ref|live-pane|dead-pane|dead-pane-loaded|rebound-thread|
    unmeasurable``), and on ``rebound-thread`` the registry row has already
    been rewritten to the thread lane. An absent binary, a timeout, or
    unparseable output is ``("unmeasurable", <cause>, None)`` so the caller
    fails open exactly like the probe contract.
    """
    binary = rust_binary.resolve_installed_binary()
    if binary is None:
        return ("unmeasurable", "binary-absent", None)
    try:
        proc = subprocess.run(
            [str(binary), "lane-heal", "--session", session_id],
            capture_output=True,
            text=True,
            timeout=_MAIL_INJECT_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError):
        return ("unmeasurable", "spawn-failed", None)
    try:
        parsed = json.loads(proc.stdout.strip())
        pane = parsed.get("pane")
        return (
            str(parsed.get("verdict") or "unmeasurable"),
            parsed.get("reason"),
            pane if isinstance(pane, dict) else None,
        )
    except (ValueError, AttributeError):
        return ("unmeasurable", "unparseable-output", None)
