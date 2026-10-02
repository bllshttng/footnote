"""The decision-id shape a plan's citations must carry.

Shape only: it says nothing about whether such a decision exists. Lived in
the deleted decide engine until that family ported to Rust; its one caller is the plan
schema's decision_id validator.
"""

from __future__ import annotations

import re

_DECISION_ID_RE = re.compile(r"^d-[0-9a-f]{4,32}$", re.IGNORECASE)


def looks_like_decision_id(token: str) -> bool:
    """Is this argument shaped like a decision id rather than a subject?"""
    return bool(_DECISION_ID_RE.match(token.strip()))
