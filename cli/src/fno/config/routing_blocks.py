"""The routing schema blocks: model rows and the routing block.

The config hub (``fno.config``) is over the file budget and shrink-only;
these live here by the same ruling that moved the spawn-defaults blocks to
``spawn_blocks``. Re-exported from ``fno.config`` for every reader.
"""

from typing import Optional

from pydantic import BaseModel, ConfigDict, Field, field_validator


class RoutingModelBlock(BaseModel):
    """One declared model row (nested under 'config.routing.models'): the
    invocation facts a benchmark snapshot cannot carry. ``name`` is the join
    key and later rows override per field. Field prose:
    docs/architecture/role-based-model-routing.md."""

    model_config = ConfigDict(extra="ignore")

    name: str = ""
    harness: str = ""
    model: str = ""
    # route names a vendor lane, account a provider record id: both say
    # which ACCOUNT'S quota the row spends.
    route: str = ""
    account: str = ""
    band: str = ""
    effort: str = ""
    cost_per_mtok_in: Optional[float] = None
    context: Optional[int] = None
    # The sideline lane color when this row matches an agent (x-1b35).
    color: str = ""
    # The verified native view for this access path; empty is unverified,
    # invisible to strict routing while remote or unknown.
    operator_view: str = ""


class RoutingBlock(BaseModel):
    """Config-first model routing inventory (nested under 'config.routing').

    Config overrides the built-in fallback table per model and per field and
    extends it; the fallback only keeps a tier request answerable where
    nothing is declared. Full policy: docs/architecture/role-based-model-routing.md."""

    model_config = ConfigDict(extra="ignore")

    objective: str = "cheapest-that-clears"
    prefer_harness: str = ""
    models: list[RoutingModelBlock] = Field(default_factory=list)
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


