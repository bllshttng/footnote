"""Transport for the ``spawn-overlay`` verb: the route-slot shape. One JSON
payload in, one parsed JSON answer out. A missing, failing or malformed
answer raises :class:`SpawnOverlayUnavailable` - a named refusal, never a
silent spawn, exactly the posture route-slot set."""

from __future__ import annotations

import json
import subprocess
from typing import Any

from fno.rust_binary import find_dev_binary, resolve_binary


class SpawnOverlayUnavailable(RuntimeError):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def spawn_overlay_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip: JSON payload in, parsed JSON answer out."""
    import os

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        raise SpawnOverlayUnavailable(
            "the fno-agents binary was not found; reinstall fno,"
            " run `fno doctor update --rust`, or set FNO_AGENTS_BIN"
        )
    try:
        proc = subprocess.run(
            [str(binary), "spawn-overlay"],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise SpawnOverlayUnavailable(f"fno agents spawn-overlay failed: {exc}") from exc
    if proc.returncode != 0:
        raise SpawnOverlayUnavailable(
            f"fno agents spawn-overlay exited {proc.returncode}: {proc.stderr.strip()[:200]}"
        )
    try:
        return json.loads(proc.stdout)
    except ValueError as exc:
        raise SpawnOverlayUnavailable(
            f"fno agents spawn-overlay bad output: {exc}"
        ) from exc
    finally:
        if os.environ.get("FNO_ROUTE_SLOT_DEBUG"):
            print(json.dumps({"payload": payload}), flush=True)
