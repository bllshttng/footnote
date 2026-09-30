"""The pr config block, apart from the config monolith.

`fno/config/__init__.py` is over budget and may only shrink; the
ready-for-review knob lives here the way `_active_backlog.py` does.
"""

from pydantic import BaseModel, ConfigDict, field_validator


class PrBlock(BaseModel):
    """PR lifecycle settings (nested under 'config.pr').

    open_ready (default True) is the ready-for-review rule as a knob: PRs
    open ready, never draft, unless an operator law row spares one. The
    gh proxy's draft guard and the pr-watch sweep both read it.
    """

    model_config = ConfigDict(extra="ignore")

    open_ready: bool = True

    @field_validator("open_ready", mode="before")
    @classmethod
    def _coerce_open_ready(cls, v: object) -> bool:
        from fno.config import _coerce_bool_default_true

        return _coerce_bool_default_true(v)
