"""Transport for the ``fno-agents route-slot`` verb: a ready JSON payload in,
the parsed JSON answer out. The payload is assembled by the data owners
(``fno.route_resolve``); chain strings come back verbatim and are never
reworded here. A missing, failing or malformed answer raises
:class:`RouteSlotUnavailable` - a named refusal, never a silent spawn.
"""
from __future__ import annotations

import json
import subprocess
from typing import Any, Optional

from fno.rust_binary import find_dev_binary, resolve_binary


class RouteSlotUnavailable(RuntimeError):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def _binary_or_raise():
    """The dev checkout's own build outranks any installed copy: testing
    against a stale PATH binary would resolve with last release's vocabulary."""
    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        raise RouteSlotUnavailable(
            "the fno-agents binary was not found; reinstall fno,"
            " run `fno doctor update --rust`, or set FNO_AGENTS_BIN"
        )
    return binary


def _route_slot_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip: JSON payload in, parsed JSON answer out."""
    import os

    try:
        proc = subprocess.run(
            [str(_binary_or_raise()), "route-slot"],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RouteSlotUnavailable(f"fno-agents route-slot failed: {exc}") from exc
    if proc.returncode != 0:
        raise RouteSlotUnavailable(
            f"fno-agents route-slot exited {proc.returncode}: {proc.stderr.strip()[:200]}"
        )
    try:
        return json.loads(proc.stdout)
    except ValueError as exc:
        raise RouteSlotUnavailable(f"fno-agents route-slot bad output: {exc}") from exc
    finally:
        if os.environ.get("FNO_ROUTE_SLOT_DEBUG"):
            print(json.dumps({"payload": payload}), flush=True)


def route_slot(payload: dict[str, Any]) -> tuple[Optional[dict], list[str]]:
    """The slot/grid legs: returns ``(candidate, chain)``."""
    out = _route_slot_call(payload)
    return out.get("candidate"), [str(line) for line in (out.get("chain") or [])]


def route_tier(payload: dict[str, Any]) -> tuple[Optional[str], list[str]]:
    """The tier leg: returns ``(model, chain)``."""
    out = _route_slot_call(payload)
    return out.get("model"), [str(line) for line in (out.get("chain") or [])]


def route_states(payload: dict[str, Any]) -> dict[str, Any]:
    """The readout leg: the verb's whole states answer (lane_states, chain,
    the policy lines, and would_take)."""
    return _route_slot_call(payload)
