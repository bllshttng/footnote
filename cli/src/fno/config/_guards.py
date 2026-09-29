"""Guardrail preset (``guards.*``) - how many incident guards a project inherits.

The preset logic lives in Rust (``guard_enabled`` in crates/fno-agents); this
model only carries the value. ``FNO_GUARD_PRESET`` wins over the config key.
"""

from typing import Literal

from pydantic import BaseModel, ConfigDict


class GuardsBlock(BaseModel):
    """Which gatable guards run for this project.

    ``strict`` runs every gatable guard, ``standard`` keeps the two
    destructive-write guards on, ``off`` runs none. The state-integrity guards
    never gate in any preset. A malformed value reads as ``strict`` in Rust.
    """

    model_config = ConfigDict(extra="ignore")

    preset: Literal["strict", "standard", "off"] = "standard"
