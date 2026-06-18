"""Notion reality-check stub.

Returns a structured not-implemented response. Exit 0 is intentional -
callers (gate verify, dispatcher) need a structured "not yet" response,
not a crash. Real Notion integration ships with a domain plan later.
"""
from __future__ import annotations

from typing import Any, Dict


def check_notion(**kwargs: Any) -> Dict[str, Any]:
    """Notion reality-check stub.

    Args:
        **kwargs: Any arguments (ignored).

    Returns:
        {ok: false, error: {kind: "not-implemented", domain: "notion"}}
    """
    return {"ok": False, "error": {"kind": "not-implemented", "domain": "notion"}}
