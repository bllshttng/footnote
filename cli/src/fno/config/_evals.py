"""Eval-suite worker + demand settings (the ``config.evals`` block).

Its own module because ``config/__init__.py`` is over the shrink-only budget.
"""

from __future__ import annotations

import logging

from pydantic import BaseModel, ConfigDict, field_validator, model_validator

_LOG = logging.getLogger(__name__)


class EvalsBlock(BaseModel):
    """Eval-suite settings: the grading-worker gate plus the demand keys.
    Keys documented in the generated reference and docs/evals.md.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        # Lazy import: the package module imports this block at startup.
        from fno.config import _coerce_bool_default_true

        return _coerce_bool_default_true(v)

    schedule_days: int = 7
    stale_days: int = 7

    @model_validator(mode="before")
    @classmethod
    def _sanitize_days(cls, v: object) -> object:
        """Drop a bad day value so the default applies; never break load_settings()."""
        if not isinstance(v, dict):
            return v
        out = dict(v)
        for key in ("schedule_days", "stale_days"):
            if key not in out:
                continue
            raw = out[key]
            try:
                ok = not isinstance(raw, bool) and int(raw) >= 0
            except (TypeError, ValueError):
                ok = False
            if not ok:
                _LOG.warning("config.evals.%s=%r invalid; using default", key, raw)
                out.pop(key)
        return out
