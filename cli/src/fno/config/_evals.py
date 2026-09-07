"""Eval-suite worker + demand settings (the ``config.evals`` block).

Lives beside :mod:`fno.config._watchdog`: the block is small but the module
that used to host it is over the file budget and may only shrink.
"""

from __future__ import annotations

import logging

from pydantic import BaseModel, ConfigDict, field_validator, model_validator

_LOG = logging.getLogger(__name__)


class EvalsBlock(BaseModel):
    """Eval-suite worker settings (nested under 'config.evals').

    x-aaaf wave 2: `evals/runner.py`'s `run_task` spawns a headless grading
    worker with no enable key at all. Default ``True`` matches its CURRENT
    effective behavior (it always ran when invoked) - shipping this gate
    changes nothing until an operator explicitly disables it.

    x-ab72 adds the demand side: ``stale_days`` is the age at which the newest
    regression-tier run reads STALE in `fno doctor` and `fno backlog triage
    health`; ``schedule_days`` is how often the pr-watch tick runs the
    regression tier (0 disables the scheduled run).
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
        """Drop a non-integer or negative day value so the field default applies.

        A malformed value must never break load_settings(); it degrades to the
        modeled default for that key (with a WARNING), the block's existing
        fail-safe posture.
        """
        if not isinstance(v, dict):
            return v
        out = dict(v)
        for key in ("schedule_days", "stale_days"):
            if key not in out:
                continue
            raw_val = out[key]
            if isinstance(raw_val, bool):
                ok = False
            else:
                try:
                    ok = int(raw_val) >= 0
                except (TypeError, ValueError):
                    ok = False
            if not ok:
                _LOG.warning(
                    "config.evals.%s=%r invalid; using default", key, raw_val
                )
                out.pop(key)
        return out
