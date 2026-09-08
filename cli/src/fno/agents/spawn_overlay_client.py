"""Transport for the ``spawn-overlay`` verb: the route-slot shape. One JSON
payload in, one parsed JSON answer out. A missing, failing or malformed
answer raises :class:`SpawnOverlayUnavailable` - a named refusal, never a
silent spawn, exactly the posture route-slot set."""

from __future__ import annotations

from typing import Any

from fno.rust_binary import VerbUnavailable, verb_call


class SpawnOverlayUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def spawn_overlay_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip: JSON payload in, parsed JSON answer out."""
    return verb_call("spawn-overlay", payload, SpawnOverlayUnavailable)
