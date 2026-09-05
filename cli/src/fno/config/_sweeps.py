"""Knobs the daemon's periodic sweeps read.

Reap-receipt retention, the single-flight latch, and the orphan reaper share
one job: keeping the machine from filling up with work nobody is waiting on.

The flat keys ride a mixin rather than a nested block because they are read as
``agents.single_flight_ttl_seconds``. The Rust daemon resolves the same three
in ``agents_config.rs``; this model keeps ``fno config get`` and ``fno config
doctor`` honest about them.
"""
from __future__ import annotations

from pydantic import BaseModel, ConfigDict, field_validator

#: Keys whose configured value could not be read and fell back to the model
#: default, recorded during ``load_settings`` and printed by
#: ``fno config doctor``. A degrade nobody can see is indistinguishable from a
#: value that was never set.
DEGRADED: dict[str, str] = {}


class ReapReceiptsBlock(BaseModel):
    """Retention for the reap-receipt store (nested under 'config.agents').

    A reaped row's resume handle lives at ``~/.fno/reap-receipts/`` for this
    many days, then the GC sweep expires it. A receipt whose ``reaped_at``
    cannot be read is kept and named in the sweep summary - a failed read is
    not evidence of age.
    """

    model_config = ConfigDict(extra="ignore")

    retain_days: int = 7


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
    def _coerce_seconds(cls, value: object, info) -> object:
        """A bad value degrades to the default and is recorded for doctor.

        Raising here would fail ``load_settings()`` for the whole process, so
        one typo would make every ``fno`` command exit. Degrade toward the
        working default and make the mistake visible instead.
        """
        default = cls.model_fields[info.field_name].default
        try:
            seconds = int(value)  # type: ignore[call-overload]
        except (TypeError, ValueError):
            DEGRADED[f"agents.{info.field_name}"] = repr(value)
            return default
        if seconds <= 0:
            DEGRADED[f"agents.{info.field_name}"] = repr(value)
            return default
        return seconds
