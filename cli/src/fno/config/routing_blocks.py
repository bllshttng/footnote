"""The routing schema block: the routing block itself.

The config hub (``fno.config``) is over the file budget and shrink-only;
this lives here by the same ruling that moved the spawn-defaults blocks to
``spawn_blocks``. Re-exported from ``fno.config`` for every reader.
"""

from typing import Any

from pydantic import BaseModel, ConfigDict, Field, field_validator


class RoutingBlock(BaseModel):
    """Config-first model routing inventory (nested under 'config.routing').

    Config overrides the built-in fallback table per model and per field and
    extends it; the fallback only keeps a tier request answerable where
    nothing is declared. Full policy: docs/architecture/role-based-model-routing.md."""

    model_config = ConfigDict(extra="ignore")

    objective: str = "cheapest-that-clears"
    prefer_harness: str = ""
    # Declared rows stay plain mappings, handed to readers verbatim. The row
    # shape is {name, harness, model, route, account, band, effort,
    # operator_view, cost_per_mtok_in, context, color}; a repeated name folds
    # per field. The one seam boundary (_field) reads the mapping spelling -
    # a type test at a reader is the trap (x-947c), never the cure.
    models: list[dict[str, Any]] = Field(default_factory=list)
    # Opt-in strict inventory (default off): a spawn qualifies against the
    # work-kind slot's declared lanes only; a pin constrains, never bypasses.
    enforce_inventory: bool = False
    # The operator's access posture: local, remote, or unknown (filters like
    # remote). Declared through config, never inferred.
    operator_access: str = "unknown"

    @field_validator("objective", mode="before")
    @classmethod
    def _coerce_objective(cls, v: object) -> object:
        """Only the three literals are honored; anything else degrades to the
        default. Same degrade-toward-safety stance as DispatchBlock: a typo can
        never select an objective the operator did not name."""
        return v if v in ("cheapest-that-clears", "best-available", "prefer-harness") else (
            "cheapest-that-clears"
        )
