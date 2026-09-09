"""Graph-store rollout switches shared by the Python client and Rust keeper."""
from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict


class GraphBlock(BaseModel):
    """Reversible graph-store migration controls."""

    model_config = ConfigDict(extra="ignore")

    commit_mode: Literal["rows", "whole"] = "rows"
