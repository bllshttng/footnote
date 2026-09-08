"""The spawn-defaults schema blocks. Split from ``fno.config`` (a shrink-only
file) and re-exported from there, so every reader keeps its import path."""

from __future__ import annotations

from typing import Any, List

from pydantic import BaseModel, ConfigDict, Field, field_validator


class HarnessOverlayBlock(BaseModel):
    """One harness's answers under a spawn-defaults block.

    Precedence: explicit flag > lane > ``profiles.<verb>.harness.<h>`` >
    ``profiles.<verb>`` > ``defaults.harness.<h>`` > ``defaults``. A
    permission/effort value spells the HARNESS's own flags, so the answer
    lives keyed by harness; ranking fields are refused at the spawn seam.
    ``extra="allow"`` is deliberate: a smuggled lane field must survive load
    so the seam can name it in its refusal.
    """

    model_config = ConfigDict(extra="allow")

    permission_mode: str = ""
    effort: str = ""
    substrate: str = ""
    args: List[str] = Field(default_factory=list)


class SpawnDefaultsBlock(BaseModel):
    """Default spawn routing (nested under 'config.agents.defaults').

    The bottom-most operator rung of the spawn precedence chain: an explicit
    CLI flag > these defaults > the built-in. Every bare `fno agents spawn`
    inherits any field set here, autonomous dispatch included; an unpinned
    `model` or `effort` DOES inherit. Empty string = unset. No value
    validation here: config stays a leaf module (x-7fdd); provider and effort
    are checked at the spawn seam. `route`/`account` sit beside the legacy
    `provider` field (ruling 4), which keeps meaning harness.
    """

    model_config = ConfigDict(extra="ignore")

    provider: str = ""
    model: str = ""
    effort: str = ""
    # Config-sourced values degrade open at the spawn seam; an explicit flag
    # stays fail-closed. Empty = unset.
    substrate: str = ""
    permission_mode: str = ""
    # route carries vendor/model as vendor/model (forwarded as --route, so it
    # fails closed downstream on an unknown vendor); account forwards
    # --account. Neither name carries an axis word, so the four-axis guard
    # never reads them as bindings.
    route: str = ""
    account: str = ""
    # Per-harness answers (x-8975): the scalars above are the base; an entry
    # here re-answers one harness whose flag vocabulary differs.
    harness: dict[str, HarnessOverlayBlock] = Field(default_factory=dict)

    @field_validator("harness", mode="before")
    @classmethod
    def _coerce_harness_overlays(cls, v: object) -> object:
        """A non-mapping overlay table degrades to empty: one typo must never
        brick every command at load, mirroring ``_coerce_profiles``."""
        if not isinstance(v, dict):
            return {}
        return {k: val for k, val in v.items() if isinstance(val, dict)}


class SpawnProfileBlock(SpawnDefaultsBlock):
    """Per-verb overlay plus its strict ordered delivery-lane vocabulary."""

    pane_group: str = ""
    # Kept raw so a malformed routing list cannot fail every config read; the
    # spawn seam validates and refuses before launching anything.
    lanes: Any = Field(default_factory=list)
