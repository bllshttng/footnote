"""Split placement config (nested under 'config.split'); a file-budget module."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict, field_validator


class SplitBlock(BaseModel):
    """Split placement config (nested under 'config.split')."""

    model_config = ConfigDict(extra="ignore")

    # Where the mux row menu's Split Direction toggle starts; mirrors the Rust
    # reader's tolerance (crates/fno digest_overlay).
    opens: Literal["pane", "portal"] = "pane"

    @field_validator("opens", mode="before")
    @classmethod
    def _coerce_opens(cls, v: object) -> object:
        """Unknown values degrade to the pane default, never error."""
        return "portal" if isinstance(v, str) and v.strip().lower() == "portal" else "pane"
