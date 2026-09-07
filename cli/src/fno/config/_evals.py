"""Eval-suite worker + demand settings (the ``config.evals`` block).

Its own module because ``config/__init__.py`` is over the shrink-only file
budget, the same reason the watchdog block lives in ``_watchdog.py``.
"""

from __future__ import annotations

import logging

from pydantic import BaseModel, ConfigDict, field_validator, model_validator

_LOG = logging.getLogger(__name__)


class EvalsBlock(BaseModel):
    """Eval-suite settings: the grading-worker gate plus the demand keys.

    ``enabled`` gates the headless grading-worker spawn (x-aaaf wave 2) and
    defaults True: the spawner ran unconditionally before the gate existed.
    ``schedule_days`` is how often the regression tier runs on a schedule (0
    disables); ``stale_days`` is the age at which the newest regression-tier
    run reads STALE in `fno doctor` and `fno backlog triage health`.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        # Lazy: the helper lives in the package module, which imports this
        # block at startup - a module-level import here would be circular.
        from fno.config import _coerce_bool_default_true

        return _coerce_bool_default_true(v)

    schedule_days: int = 7
    stale_days: int = 7

    @model_validator(mode="before")
    @classmethod
    def _sanitize_days(cls, v: object) -> object:
        """Drop a bad day value so the default applies; never break load."""
        if not isinstance(v, dict):
            return v
        out = dict(v)
        for key in ("schedule_days", "stale_days"):
            raw = out.get(key)
            if key not in out:
                continue
            ok = not isinstance(raw, bool)
            if ok:
                try:
                    ok = int(raw) >= 0
                except (TypeError, ValueError):
                    ok = False
            if not ok:
                _LOG.warning("config.evals.%s=%r invalid; using default", key, raw)
                out.pop(key)
        return out
