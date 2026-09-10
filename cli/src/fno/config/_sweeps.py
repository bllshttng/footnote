"""Config models for daemon sweep knobs mirrored by the Rust runtime."""
from __future__ import annotations

from pydantic import BaseModel, ConfigDict, field_validator
from pydantic_core.core_schema import ValidationInfo

#: Keys whose configured value could not be read and fell back to the model
#: default, recorded during ``load_settings`` and printed by
#: ``fno config doctor``. A degrade nobody can see is indistinguishable from a
#: value that was never set.
DEGRADED: dict[str, str] = {}


class ReapReceiptsBlock(BaseModel):
    """Retention for readable, aged reap receipts under ``config.agents``."""

    model_config = ConfigDict(extra="ignore")

    retain_days: int = 7


class ReapBlock(BaseModel):
    """Scope of the roster-side sweep (nested under 'config.agents.reap').

    ``roster_scope`` names which claude rows the sweep may retire:
    ``off`` retires nothing, ``provenanced`` (the default) only rows whose
    provenance resolves to fno and whose work is done, ``all`` widens to
    rows fno itself spawned (sessions or registry provenance) with open
    work. One rule no value can cross: a row that resolves to no fno node
    is never retirable at any value, including ``all`` - an operator's
    hand-started session is safe by construction, not by default value.
    """

    model_config = ConfigDict(extra="ignore")

    roster_scope: str = "provenanced"

    @field_validator("roster_scope", mode="before")
    @classmethod
    def _coerce_roster_scope(cls, value: object, info: ValidationInfo) -> object:
        """Lowercase and accept only the three scope values.

        A bad value degrades to the default and is recorded for doctor
        (same idiom as SweepKeys): raising would fail load_settings() for
        the whole process, and one typo must not widen the sweep.
        """
        name = info.field_name or ""
        default = cls.model_fields[name].default
        if not isinstance(value, str):
            DEGRADED[f"agents.reap.{name}"] = repr(value)
            return default
        lowered = value.strip().lower()
        if lowered in {"off", "provenanced", "all"}:
            return lowered
        DEGRADED[f"agents.reap.{name}"] = repr(value)
        return default


class StateReapBlock(BaseModel):
    """Retention windows for expendable local state families."""

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True
    locks_retain_days: int = 7
    expired_claims_retain_days: int = 30
    pr_status_cache_retain_days: int = 14

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, value: object) -> object:
        if isinstance(value, bool):
            return value
        DEGRADED["agents.state_reap.enabled"] = repr(value)
        return cls.model_fields["enabled"].default

    @field_validator(
        "locks_retain_days",
        "expired_claims_retain_days",
        "pr_status_cache_retain_days",
        mode="before",
    )
    @classmethod
    def _coerce_days(cls, value: object, info: ValidationInfo) -> object:
        name = info.field_name or ""
        default = cls.model_fields[name].default
        if isinstance(value, int) and not isinstance(value, bool) and value > 0:
            return value
        DEGRADED[f"agents.state_reap.{name}"] = repr(value)
        return default


class SweepKeys(BaseModel):
    """Flat ``config.agents.*`` seconds that the sweeps read.

    ``single_flight_ttl_seconds`` is how long one child's written answer counts
    as fresh, so five callers arriving inside it cost one child.

    ``single_flight_join_budget_seconds`` is how long a later caller waits for
    the holder's answer before running its own. It sits over the 23.2 s
    worst-measured roster read on purpose: load is when the latch has to hold.

    ``orphan_reap_after_seconds`` is the age at which a child that init
    inherited is reaped. 90 minutes is derived, not picked: the longest
    legitimate detached child is ``do pr wait --timeout 30m``, so the threshold
    is three times the longest thing allowed to be running.
    """

    single_flight_ttl_seconds: int = 10
    single_flight_join_budget_seconds: int = 30
    orphan_reap_after_seconds: int = 5400

    @field_validator(
        "single_flight_ttl_seconds",
        "single_flight_join_budget_seconds",
        "orphan_reap_after_seconds",
        mode="before",
    )
    @classmethod
    def _coerce_seconds(cls, value: object, info: ValidationInfo) -> object:
        """A bad value degrades to the default and is recorded for doctor.

        Raising here would fail ``load_settings()`` for the whole process, so
        one typo would make every ``fno`` command exit. Degrade toward the
        working default and make the mistake visible instead.
        """
        name = info.field_name or ""
        default = cls.model_fields[name].default
        try:
            seconds = int(value)  # type: ignore[call-overload]
        except (TypeError, ValueError):
            seconds = 0
        if seconds > 0:
            return seconds
        DEGRADED[f"agents.{name}"] = repr(value)
        return default
