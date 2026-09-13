"""The ``config.routing.admission`` block: opt-in shared-account capacity
reservations before a routing-lane launch.

Subscription-percent admission only (x-1afa): ``demand_pct`` estimates one
dispatch's share of a quota window, ``reserve_pct`` protects a share of the
same window for difficult work and reviews. Percentages are the only unit this
block speaks - a dollar figure is a different resource and is refused at the
admission seam rather than compared to a percentage. API spend forecasting
stays unsupported; API accounts participate through ``max_inflight_per_pool``
only.

Nothing here raises at load: one typo in an opt-in block must not take down
every command (the ``_sweeps`` idiom). Bad values degrade to the default and
are recorded in ``DEGRADED``; ``resolve_admission_policy`` hands them to the
admission owner, which refuses an armed-but-tainted policy with the exact
field-and-unit error.
"""
from __future__ import annotations

import dataclasses
import re
from typing import Any

from pydantic import BaseModel, ConfigDict, Field, model_validator

#: Keys whose configured value could not be read and fell back to the model
#: default, recorded during ``load_settings`` and printed by
#: ``fno config doctor`` (same ledger as ``fno.config._sweeps``).
DEGRADED: dict[str, str] = {}

#: The prefix every entry this module writes carries.
_DEGRADED_PREFIX = "routing.admission."

#: The one vocabulary difficulty keys may use. Anything else is a config error.
DIFFICULTIES = ("default", "low", "medium", "high")

_CURRENCY_MARKER = re.compile(r"[$€£]|usd", re.IGNORECASE)

_PERCENT_TABLES = ("demand_pct", "reserve_pct")


def _admission_config_error(field: str, message: str) -> None:
    DEGRADED[f"{_DEGRADED_PREFIX}{field}"] = message


def admission_config_errors() -> dict[str, str]:
    """The recorded ``routing.admission.*`` config errors, exact field first."""
    return {
        key: message
        for key, message in DEGRADED.items()
        if key.startswith(_DEGRADED_PREFIX)
    }


def _unit_error(table: str, where: str, value: object) -> str:
    """The exact field-and-unit sentence a percentage refusal carries."""
    if isinstance(value, str) and _CURRENCY_MARKER.search(value):
        return (
            f"routing.admission.{table}.{where}: {value!r} is a currency amount; "
            "admission counts subscription-window percentages (0..100), not "
            "dollars - API spend forecasting is unsupported"
        )
    return (
        f"routing.admission.{table}.{where}: {value!r} is not a subscription "
        "percentage in [0, 100]"
    )


class RoutingAdmissionBlock(BaseModel):
    """Opt-in admission policy (nested under 'config.routing.admission').

    ``enabled`` is false by default: a fresh install reserves nothing and
    dispatch behaves exactly as before. Every percentage is a share of ONE
    subscription quota window; windows are conjunctive and are never averaged
    or added.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    #: Concurrent reserved+committed dispatches allowed per quota pool,
    #: regardless of percentages - the backstop that bounds an estimate.
    max_inflight_per_pool: int = 3
    #: How long one reservation survives without a proven worker (seconds).
    #: A live worker past its TTL is revalidated, never refunded.
    reservation_ttl_seconds: int = 900
    #: verb -> difficulty -> percent of the window this dispatch may consume.
    demand_pct: dict[str, dict[str, Any]] = Field(default_factory=dict)
    #: verb -> difficulty -> percent of the window held back for difficult
    #: work and reviews. A configured priority exception may consume it; known
    #: exhaustion is never configured away here.
    reserve_pct: dict[str, dict[str, Any]] = Field(default_factory=dict)

    @model_validator(mode="before")
    @classmethod
    def _record_unknown_fields(cls, v: object) -> object:
        if isinstance(v, dict):
            known = set(cls.model_fields)
            unknown = sorted(str(k) for k in v if k not in known)
            if unknown:
                _admission_config_error(
                    "fields",
                    "routing.admission: unknown field(s) "
                    f"{', '.join(unknown)}; known: {', '.join(sorted(known))}",
                )
        return v

    @model_validator(mode="after")
    def _validate_armed(self) -> "RoutingAdmissionBlock":
        """Validate in place, never raise.

        A disabled block gates nothing, so its typos stay untouched. An armed
        block gets its bad leaves stripped to the defaults with the exact
        error recorded, so ``resolve_admission_policy`` can refuse at the
        seam instead of loading a policy the operator did not write.
        """
        if self.enabled is not True:
            return self
        if not isinstance(self.max_inflight_per_pool, int) or isinstance(
            self.max_inflight_per_pool, bool
        ) or self.max_inflight_per_pool < 1:
            _admission_config_error(
                "max_inflight_per_pool",
                f"routing.admission.max_inflight_per_pool: "
                f"{self.max_inflight_per_pool!r} is not a positive concurrency "
                "bound",
            )
            self.max_inflight_per_pool = 3
        if not isinstance(self.reservation_ttl_seconds, int) or isinstance(
            self.reservation_ttl_seconds, bool
        ) or self.reservation_ttl_seconds < 1:
            _admission_config_error(
                "reservation_ttl_seconds",
                f"routing.admission.reservation_ttl_seconds: "
                f"{self.reservation_ttl_seconds!r} is not a positive TTL",
            )
            self.reservation_ttl_seconds = 900
        for name in _PERCENT_TABLES:
            raw = getattr(self, name)
            clean: dict[str, dict[str, float]] = {}
            if not isinstance(raw, dict):
                _admission_config_error(
                    name, f"routing.admission.{name}: expected a verb -> difficulty table"
                )
                setattr(self, name, {})
                continue
            for verb, table in raw.items():
                if not isinstance(verb, str) or not isinstance(table, dict):
                    _admission_config_error(
                        name,
                        f"routing.admission.{name}.{verb!r}: expected a "
                        "difficulty table",
                    )
                    continue
                inner: dict[str, float] = {}
                for difficulty, pct in table.items():
                    where = f"{verb}.{difficulty}"
                    if difficulty not in DIFFICULTIES:
                        _admission_config_error(
                            name,
                            f"routing.admission.{name}.{where}: unknown "
                            f"difficulty {difficulty!r}; known: "
                            f"{', '.join(DIFFICULTIES)}",
                        )
                        continue
                    if (
                        isinstance(pct, bool)
                        or not isinstance(pct, (int, float))
                        or not 0.0 <= float(pct) <= 100.0
                    ):
                        _admission_config_error(name, _unit_error(name, where, pct))
                        continue
                    inner[difficulty] = float(pct)
                if inner:
                    clean[verb] = inner
            setattr(self, name, clean)
        return self


@dataclasses.dataclass(frozen=True)
class AdmissionPolicy:
    """The resolved, error-free view readers consume.

    A data bag only: the difficulty lookup and the whole decision live in
    the Rust owner (``crates/fno-agents/src/admission.rs``), which returns
    the priced axes on every receipt, so no display re-derives them.
    """

    enabled: bool = False
    max_inflight_per_pool: int = 3
    reservation_ttl_seconds: int = 900
    demand_pct: dict[str, dict[str, float]] = dataclasses.field(default_factory=dict)
    reserve_pct: dict[str, dict[str, float]] = dataclasses.field(default_factory=dict)
    #: Exact config errors recorded while the block was armed and malformed.
    #: Non-empty + enabled = the owner refuses rather than guessing.
    config_errors: dict[str, str] = dataclasses.field(default_factory=dict)


def resolve_admission_policy(settings: object = None) -> AdmissionPolicy:
    """The one policy read. Never raises: an unreadable settings object
    degrades to the disabled default, exactly like every other quota read."""
    try:
        if settings is None:
            from fno.config import load_settings

            settings = load_settings()
        block = settings.routing.admission  # type: ignore[attr-defined]
    except Exception:  # noqa: BLE001 - a broken read gates nothing
        return AdmissionPolicy()
    if not isinstance(block, RoutingAdmissionBlock) or not block.enabled:
        return AdmissionPolicy()
    return AdmissionPolicy(
        enabled=True,
        max_inflight_per_pool=block.max_inflight_per_pool,
        reservation_ttl_seconds=block.reservation_ttl_seconds,
        demand_pct={
            verb: dict(table) for verb, table in block.demand_pct.items()
        },
        reserve_pct={
            verb: dict(table) for verb, table in block.reserve_pct.items()
        },
        config_errors=admission_config_errors(),
    )
