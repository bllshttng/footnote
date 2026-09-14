"""Registry-backed liveness for `fno backlog provenance`.

The graph layer must not import the agents runtime (company boundary); this
provenance-package module is the one that reads it.
"""
from __future__ import annotations


def registry_status_index() -> "dict[str, str] | None":
    """Map ``harness_session_id`` -> registry status word for one machine.

    ``None`` means the registry could not be read: the caller then omits
    liveness rather than reading every row as reaped. An empty mapping is a
    real answer, so a row with no entry reads ``reaped``.
    """
    try:
        from fno.agents.registry import load_registry

        rows = load_registry()
    except Exception:  # noqa: BLE001 - liveness must never fail the verb
        return None
    index: dict[str, str] = {}
    for row in rows or []:
        sid = getattr(row, "harness_session_id", None)
        if sid:
            index[sid] = getattr(row, "status", None) or "unknown"
    return index
