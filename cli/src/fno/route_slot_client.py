"""Transport for the ``fno-agents route-slot`` verb: a ready JSON payload in,
the parsed JSON answer out. The payload is assembled by the data owners
(``fno.route_resolve``); chain strings come back verbatim and are never
reworded here. A missing, failing or malformed answer raises
:class:`RouteSlotUnavailable` - a named refusal, never a silent spawn.
"""
from __future__ import annotations

from typing import Any

from fno.rust_binary import VerbUnavailable, verb_call


class RouteSlotUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def route_slot_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip: JSON payload in, parsed JSON answer out."""
    return verb_call("route-slot", payload, RouteSlotUnavailable)
