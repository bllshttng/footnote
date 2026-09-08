"""The routing schema blocks: model rows and the routing block.

The config hub (``fno.config``) is over the file budget and shrink-only;
these live here by the same ruling that moved the spawn-defaults blocks to
``spawn_blocks``. Re-exported from ``fno.config`` for every reader.
"""

from typing import Optional

from pydantic import BaseModel, ConfigDict, Field, field_validator


class RoutingModelBlock(BaseModel):
    """One declared model row (nested under 'config.routing.models').

    The row carries the invocation facts a benchmark snapshot cannot: how to
    reach the model on THIS machine (harness + --model value), which band it
    runs in, and what effort surface it takes. ``name`` is the join key; the
    same name may appear more than once and later rows override per field
    (unset fields keep the earlier row's value), so a base row plus a one-field
    override is a legal declaration. Cost belongs to the ACCESS PATH, never
    to the model: the same model reached two ways (subscription credits vs API
    dollars) is TWO rows with two cost profiles, never one averaged number -
    the measured ratio between the two paths for one pair is 6x on identical
    inference. No value validation here: config stays a leaf module (x-7fdd);
    the spawn seam and the resolver validate.
    """

    model_config = ConfigDict(extra="ignore")

    name: str = ""
    harness: str = ""
    model: str = ""
    # route names a vendor lane (``zai/glm-5.3``), account names a provider
    # record id; both identify which ACCOUNT'S quota a row spends, which is how
    # capacity expands a harness to its accounts.
    route: str = ""
    account: str = ""
    band: str = ""
    effort: str = ""
    cost_per_mtok_in: Optional[float] = None
    context: Optional[int] = None
    # (x-1b35) The sideline lane color when this row matches an agent. The
    # most specific key of the lane-color cascade - an empty value (the
    # default) keeps the row out of color resolution entirely. Parsed by the
    # mux's Rust reader (crates/fno/src/sideline_color.rs); declared here so
    # a typo surfaces at config validation, not only at render time.
    color: str = ""
    # The operator view that can observe a session on this access path
    # (``claude-native`` or ``codex-native`` on this machine), set only after
    # the operator actually confirmed the view. Empty means unverified: the
    # row is invisible to strict routing while remote or unknown. The value
    # is qualification metadata, never a capability grade.
    operator_view: str = ""


class RoutingBlock(BaseModel):
    """Config-first model routing inventory (nested under 'config.routing').

    Config is the PRIMARY routing surface. A small built-in table is a
    FALLBACK under it, never the authority: config OVERRIDES it per model and
    per field and EXTENDS it with models it never named, so adding a model is
    a config edit and never a Python edit. A stranger's install can therefore
    declare its own models (local, ollama, gemini-only) and every one of them
    outranks the built-in row of the same name.

    The fallback keeps a tier request answerable on an install that declares
    nothing: review level resolves a model for every level. The GRID stays
    config-first regardless - a virgin install records
    ``no-inventory-declared`` and injects nothing, because the grid reads
    whether config DECLARED a row, not whether any row exists.

    ``fno/routing_sample.toml`` ships as a labelled sample (inside the
    package, so a wheel finds it) that no code path reads. The objective is
    itself a config key because users differ (cheapest vs strongest vs
    stay-in-a-harness).
    """

    model_config = ConfigDict(extra="ignore")

    objective: str = "cheapest-that-clears"
    prefer_harness: str = ""
    models: list[RoutingModelBlock] = Field(default_factory=list)
    # Opt-in strict inventory policy (default off: other installs keep every
    # documented default). When true, a spawn qualifies against its effective
    # work-kind slot's CONFIG-declared lanes only - explicit flags constrain
    # the choice, never bypass it, and an unresolvable request is a named
    # refusal instead of the harness default. Invalid values refuse at the
    # resolver, never coerce permissive (x-7fdd: config stays a leaf).
    enforce_inventory: bool = False
    # The operator's access posture: ``local`` (attending, native view not
    # required), ``remote`` (only verified native views qualify), or the
    # default ``unknown`` (which filters like remote and is labeled unknown
    # in every receipt). Set through the project-scoped config writer, never
    # inferred from presence, mail, or a king's opinion.
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


