"""Transport for the ``spawn-overlay`` verb: the route-slot shape. One JSON
payload in, one parsed JSON answer out. A missing, failing or malformed
answer raises :class:`SpawnOverlayUnavailable` - a named refusal, never a
silent spawn, exactly the posture route-slot set."""

from __future__ import annotations

from typing import Any

from fno.rust_binary import VerbUnavailable


class SpawnOverlayUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def spawn_overlay_call(
    payload: dict[str, Any], timeout: float = 90
) -> dict[str, Any]:
    """One subprocess round-trip: JSON payload in, parsed JSON answer out.

    The default is 90s, not verb_call's 30s: the compose's payload can name
    a node (a graph read) and a missing binary must not wedge a spawn either
    way.
    """
    # Call-time import: a module-top capture can permanently hold a test's
    # monkeypatched verb_call stub, leaking it into every later reader.
    from fno.rust_binary import verb_call

    return verb_call("spawn-overlay", payload, SpawnOverlayUnavailable, timeout=timeout)
