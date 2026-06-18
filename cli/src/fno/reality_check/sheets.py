"""Google Sheets reality-check stub.

Returns a structured not-implemented response. Exit 0 is intentional -
callers (gate verify, dispatcher) need a structured "not yet" response,
not a crash. Real Sheets integration ships with a domain plan later.
"""
from __future__ import annotations

from typing import Any, Dict


def check_sheets(**kwargs: Any) -> Dict[str, Any]:
    """Google Sheets reality-check stub.

    Args:
        **kwargs: Any arguments (ignored).

    Returns:
        {ok: false, error: {kind: "not-implemented", domain: "sheets"}}
    """
    return {"ok": False, "error": {"kind": "not-implemented", "domain": "sheets"}}
