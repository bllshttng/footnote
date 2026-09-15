"""Ask the `mail-inject --lane-heal` mode about a row's pane binding.

One Python shim over one Rust verdict mode : the door owns the
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
            [
                str(binary),
                "mail-inject",
                "--harness",
                "codex",
                "--session",
                session_id,
                "--lane-heal",
            ],
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


def raw_send_heal_action(
    verdict: str, reason: Optional[str], pane: Optional[dict], name: str
) -> tuple[str, str]:
    """Map a lane-heal verdict to the one action `mail send --raw` takes.

    Returns ``(action, detail)``: ``rebound`` means the caller re-resolves the
    registry row, ``refused`` carries the dead-pane message, ``check`` carries
    the unmeasurable message the caller shows under ``--check``, and
    ``continue`` routes normally.
    """
    if verdict == "rebound-thread":
        return ("rebound", "")
    if verdict == "dead-pane":
        label = f"{pane['session']}:{pane['pane_id']}" if pane else "unknown"
        detail = (
            f"{name!r} mux pane {label} is gone and the thread is not loaded "
            f"anywhere fno can reach ({reason}); run fno agents resume {name}"
        )
        return ("refused", detail)
    if verdict == "unmeasurable":
        return ("check", f"lane-heal could not read the pane binding ({reason})")
    return ("continue", "")
