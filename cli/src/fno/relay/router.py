"""Group 2 of the cross-session agent relay (x-908b / x-e4ac): the address
router. A ``parseAddress`` clone (the design's Router layer) that resolves a
relay address to a concrete ``session_id`` + ``provider`` + ``inject_handle``
from the persistent :mod:`fno.relay.registry`.

Three address forms:

- ``session:<id>``  -- direct session-id lookup (AC2-HP, the verifiable form).
- ``node:<id>``     -- a backlog node id/slug; resolves to that node's
                       ``session_id`` via the graph, then to its registry entry.
- ``<name>``        -- a bare friendly handle; matched against entry ``name``.

A miss raises :class:`Unroutable` carrying the ``relay_unroutable{<addr>}``
marker -- the design's Failure Mode boundary forbids silently swallowing an
unknown target. No daemon, no injection here; this is pure resolution.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Optional

from fno.relay import registry as _registry
from fno.relay.registry import RegistryEntry


class Unroutable(Exception):
    """No live registry entry matches the address."""


@dataclass(frozen=True)
class Address:
    kind: str  # "session" | "node" | "name"
    value: str


@dataclass(frozen=True)
class Resolution:
    session_id: str
    provider: str
    inject_handle: Optional[str]


def parse_address(raw: str) -> Address:
    """Split ``session:<id>`` / ``node:<id>`` / bare ``<name>``. A ``kind:``
    prefix with an empty value (``session:``) is a malformed address, not a
    bare name -- it is rejected so a typo can't silently become a name lookup."""
    raw = (raw or "").strip()
    if not raw:
        raise ValueError("empty relay address")
    for kind in ("session", "node"):
        prefix = kind + ":"
        if raw.startswith(prefix):
            value = raw[len(prefix):].strip()
            if not value:
                raise ValueError(f"malformed relay address: {raw!r}")
            return Address(kind=kind, value=value)
    return Address(kind="name", value=raw)


def _default_node_resolver(node_id: str) -> Optional[str]:
    """Resolve a backlog node id/slug to its recorded ``session_id`` via the
    graph. Lazy import so the registry/router carry no hard graph dependency
    (and tests can inject a fake resolver)."""
    try:
        from fno.graph.store import read_graph
        graph = read_graph()
    except Exception:
        return None
    for entry in graph:
        if entry.get("id") == node_id or entry.get("slug") == node_id:
            return entry.get("session_id")
    return None


def resolve(
    raw: str,
    *,
    index: Optional[dict[str, RegistryEntry]] = None,
    node_resolver: Optional[Callable[[str], Optional[str]]] = None,
) -> Resolution:
    """Resolve an address to ``(session_id, provider, inject_handle)`` or raise
    :class:`Unroutable`."""
    addr = parse_address(raw)
    idx = index if index is not None else _registry.index()

    sid: Optional[str]
    if addr.kind == "session":
        sid = addr.value
    elif addr.kind == "node":
        resolver = node_resolver or _default_node_resolver
        sid = resolver(addr.value)
        if not sid:
            raise Unroutable(f"relay_unroutable{{node:{addr.value}}}")
    else:  # name
        sid = next((s for s, e in idx.items() if e.name == addr.value), None)
        if not sid:
            raise Unroutable(f"relay_unroutable{{{addr.value}}}")

    entry = idx.get(sid)
    if entry is None:
        raise Unroutable(f"relay_unroutable{{{raw}}}")
    return Resolution(
        session_id=entry.session_id,
        provider=entry.provider,
        inject_handle=entry.inject_handle,
    )
