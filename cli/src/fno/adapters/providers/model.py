"""Data models for provider rotation substrate.

Phase 01 of the provider rotation substrate (ab-256f6b6e).
Only data shapes; no CLI surface, no loop wiring.
"""
from __future__ import annotations

from pathlib import Path
from typing import Literal, Optional

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


_RECOGNIZED_API_KEY_NAMES = frozenset(
    {"ANTHROPIC_API_KEY", "GEMINI_API_KEY", "OPENAI_API_KEY"}
)

_CLI_LITERAL = Literal["claude", "gemini", "codex", "openclaw", "hermes"]
_AUTH_LITERAL = Literal["oauth_dir", "api_key"]

_ID_PATTERN = r"^[a-z][a-z0-9-]{0,63}$"


class Pricing(BaseModel):
    """Per-provider rate card.

    Phase 02 of provider rotation failover (ab-9728b70b). The schema slot
    is pinned now so per-segment cost attribution (Spec 2.5) can apply
    these rates without a follow-up shape change. v0 only validates the
    numbers; the math that consumes them lands in 2.5.
    """

    model_config = ConfigDict(extra="forbid")

    input_per_million_usd: float = Field(..., ge=0.0)
    output_per_million_usd: float = Field(..., ge=0.0)
    cache_read_per_million_usd: float | None = Field(default=None, ge=0.0)
    cache_write_per_million_usd: float | None = Field(default=None, ge=0.0)


class AgentProviderBinding(BaseModel):
    """A per-agent provider pin in config.agents.<name>.

    Value is an object (not a bare string) so future fields (e.g. fallback,
    model_override) can land here without a schema break.

    Part of: ab-978e93ed (per-agent sigma-review routing, Spec 3).
    """

    model_config = ConfigDict(extra="forbid")

    provider: str = Field(..., pattern=_ID_PATTERN)


class ProviderRecord(BaseModel):
    """A single provider entry in config.providers.records."""

    model_config = ConfigDict(extra="forbid", str_strip_whitespace=True)

    # Required fields
    id: str = Field(..., pattern=_ID_PATTERN)
    name: str
    cli: _CLI_LITERAL
    auth: _AUTH_LITERAL
    priority: int = Field(default=100, ge=0)

    # Conditional fields (auth-strategy-dependent)
    credentials_source: Path | None = None
    env: dict[str, str] | None = None

    # Optional metadata
    account_id: str | None = None
    tags: list[str] = Field(default_factory=list)
    description: str | None = None

    # Rate card (added in failover spec phase 02). Optional; consumers
    # fall back to a hardcoded default rate card per CLI when None.
    pricing: Pricing | None = None

    # Per-provider session sub-cap (added in failover spec phase 03 task
    # 3.2). Optional; when None, only the session-level cap from
    # config.budget.{attended|unattended}.cost_cap_usd applies. When set,
    # the per-provider cost (approximate v0 math: session_cost ×
    # turns_on_provider/total_turns) is checked against this cap and the
    # stop hook trips BLOCKED with axis=per_provider when exceeded.
    cost_cap_usd_per_session: float | None = Field(default=None, ge=0.0)

    @field_validator("credentials_source", mode="after")
    @classmethod
    def _expand_credentials_source(cls, v: Optional[Path]) -> Optional[Path]:
        """Expand ~/ in credentials_source so path.exists() works as expected."""
        if v is None:
            return v
        return v.expanduser()

    @model_validator(mode="after")
    def _check_auth_strategy(self) -> "ProviderRecord":
        """Validate that auth strategy is consistent with credentials fields."""
        if self.auth == "oauth_dir":
            if self.credentials_source is None:
                raise ValueError(
                    f"auth_strategy_mismatch: {self.id}: "
                    "auth=oauth_dir requires credentials_source"
                )
        elif self.auth == "api_key":
            if not self.env:
                raise ValueError(
                    f"auth_strategy_mismatch: {self.id}: "
                    "auth=api_key requires non-empty env dict"
                )
            recognized = _RECOGNIZED_API_KEY_NAMES & set(self.env.keys())
            if not recognized:
                raise ValueError(
                    f"auth_strategy_mismatch: {self.id}: "
                    f"auth=api_key env must contain at least one of "
                    f"{sorted(_RECOGNIZED_API_KEY_NAMES)}, got {sorted(self.env.keys())}"
                )
        return self

    @model_validator(mode="after")
    def _default_account_id(self) -> "ProviderRecord":
        """Default account_id to id when not set."""
        if self.account_id is None:
            self.account_id = self.id
        return self


class FailoverConfig(BaseModel):
    """config.providers.failover block.

    Phase 03 of provider rotation failover (ab-9728b70b). Holds the
    storm-cap and any future failover-tunable knobs. The actual default
    handling lives in failover.py (DEFAULT_MAX_SWAPS_PER_PHASE) so a
    record without a failover block still works.
    """

    model_config = ConfigDict(extra="forbid")

    max_swaps_per_phase: int = Field(default=5, ge=1)


class ProvidersConfig(BaseModel):
    """Wrapper for the config.providers block plus the sibling config.agents block.

    The ``agents`` field corresponds to the top-level YAML key ``config.agents``
    (a sibling of ``config.providers``, NOT nested under it). Absent block
    returns an empty dict so callers never need a None check.

    Part of: ab-978e93ed (per-agent sigma-review routing, Spec 3).
    """

    model_config = ConfigDict(extra="forbid")

    records: list[ProviderRecord] = Field(default_factory=list)
    active: str | None = None
    failover: FailoverConfig | None = None
    # Per-agent provider pins. config.agents is a YAML sibling of
    # config.providers; absent block defaults to empty dict (back-compat).
    agents: dict[str, AgentProviderBinding] = Field(default_factory=dict)

    @model_validator(mode="after")
    def _check_unique_record_ids(self) -> "ProvidersConfig":
        """Reject configurations with duplicate record ids.

        A yaml typo that duplicates a provider id would silently use the last
        definition (last-writer-wins in by_id). This validator surfaces the
        problem at load time so users see a clear error.
        """
        seen: set[str] = set()
        duplicates: list[str] = []
        for r in self.records:
            if r.id in seen:
                duplicates.append(r.id)
            seen.add(r.id)
        if duplicates:
            raise ValueError(
                f"duplicate_record_ids: {sorted(set(duplicates))}"
            )
        return self

    @property
    def by_id(self) -> dict[str, ProviderRecord]:
        """Return records indexed by id for O(1) lookup."""
        return {r.id: r for r in self.records}


class ProviderConfigError(ValueError):
    """Raised by the loader when provider config is invalid.

    Callers catch this single exception type rather than pydantic.ValidationError.
    The message always includes the offending record id (when applicable) and
    the discriminating phrase ('auth_strategy_mismatch' or 'active_record_not_found').
    """


class ProviderStagingError(RuntimeError):
    """Raised by staging.py when a filesystem staging operation fails.

    Examples: credentials_source does not exist, or an existing symlink points
    at a different target than expected (likely user error or corruption).
    """


class ProviderNotFoundError(KeyError):
    """Raised by dispatch.py when provider_id is not in config.records.

    Distinct from ProviderUnavailableError: 'not found' means the id was never
    configured; 'unavailable' means it is configured but cannot be used right now.
    """


class ProviderUnavailableError(RuntimeError):
    """Raised by dispatch.py when a configured provider cannot be used.

    Reasons include: oauth_dir provider not staged, api_key env reference
    unresolvable (missing keychain entry, missing env var, unreadable file).
    The message always names the offending key or reference.
    """
