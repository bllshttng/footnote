"""Validation for monotonic timeout and polling budgets."""

from __future__ import annotations

import math
from typing import Optional


def validate_timeout_budget(
    timeout: float,
    *,
    label: str,
    poll: Optional[float] = None,
) -> None:
    """Reject values that can make a deadline loop non-terminating."""
    if not math.isfinite(timeout) or timeout < 0:
        raise ValueError(f"{label} timeout must be finite and non-negative")
    if poll is not None and (not math.isfinite(poll) or poll <= 0):
        raise ValueError(f"{label} poll interval must be finite and positive")
