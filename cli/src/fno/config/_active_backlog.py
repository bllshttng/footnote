"""The active_backlog config block, apart from the config monolith.

`fno/config/__init__.py` is over budget and may only shrink; the block the
drain readouts depend on lives here with the duration parser that is its alone.
"""

import re
import warnings
from typing import Optional

from pydantic import BaseModel, ConfigDict, Field, field_validator

_DURATION_RE = re.compile(r"^\s*(\d+)\s*([smhd])\s*$")
_DURATION_UNITS = {"s": 1, "m": 60, "h": 3600, "d": 86400}


def _parse_duration_to_seconds(v: object) -> Optional[int]:
    """Parse a duration string (``"5m"``/``"30s"``/``"2h"``/``"1d"``) to seconds.

    A bare int/float is interpreted as seconds. Returns ``None`` for anything
    unparseable, zero, or negative, so callers can fail safe to disabled rather
    than spin a 0-sleep hot loop (active-backlog Boundaries).
    """
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        secs = int(v)
        return secs if secs > 0 else None
    if isinstance(v, str):
        s = v.strip()
        # A bare digit string ("300") is interpreted as seconds, matching the
        # bare-int path so `interval: "300"` and `interval: 300` agree (instead
        # of the quoted form silently disabling the feature).
        if s.isdigit():
            secs = int(s)
            return secs if secs > 0 else None
        m = _DURATION_RE.match(s)
        if not m:
            return None
        secs = int(m.group(1)) * _DURATION_UNITS[m.group(2)]
        return secs if secs > 0 else None
    return None


class ActiveBacklogConfig(BaseModel):
    """Active backlog dispatcher settings (nested under 'config.active_backlog').

    The opt-in for the always-on backlog drain daemon (node x-c070): when
    enabled for a project, the per-user supervisor daemon continuously claims
    ready backlog nodes for that project and dispatches them one at a time
    through the existing megawalk loop primitive, sleeping between drains with
    an event nudge for low latency.

    Default OFF (footnote convention): a disabled block is byte-for-byte
    today's behavior. The fail-safe posture mirrors ``config.auto_continue`` /
    ``config.target.blast``: a malformed block degrades to disabled rather than
    raising out of the whole settings load, a bad scalar field is dropped to its
    default (never raised), and an invalid ``interval`` (zero, negative, or
    unparseable) disables the feature rather than spinning a 0-sleep hot loop
    (Boundaries).

    Fields
    ------
    enabled:
        ``True`` to drain every project, or a per-project map
        ``{<project>: bool}`` to scope the daemon to specific projects.
        Default ``False``. A scalar typo fails safe to disabled; in a map, only
        a clear affirmative (``true``/``yes``/``on``/``1``) enables a project.
    interval:
        Poll-floor cadence as a duration string (``"5m"``, ``"30s"``, ``"2h"``,
        ``"1d"``) or a bare integer (seconds). Default ``"5m"``. The poll floor
        is the correctness guarantee; the event nudge is a latency optimization
        layered on top.
    failure_limit:
        Consecutive dispatch failures before a node is parked (the circuit
        breaker). Default 3. Reset to zero only on a successful close.
    max_concurrent:
        GLOBAL ceiling on concurrent converge runs (``backlog advance --epic``)
        across every mission, never per mission. Default 1 (serial).
    mission: IGNORED (missions are per-epic graph state, never config; ``fno config doctor`` warns when it is set; the live axis is ``fno backlog advance --epic <id>`` / ``--stop``).
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool | dict[str, bool] = False
    interval: str = "5m"
    failure_limit: int = Field(default=3, ge=1)
    max_concurrent: int = Field(default=1, ge=1)
    mission: Optional[str] = None

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> object:
        """Fail-safe coercion for ``enabled`` (bool or per-project map).

        A bool passes through. A mapping is coerced per-value with the strict
        affirmative truth table (only ``true``/``yes``/``on``/``1`` enable a
        project; an ambiguous value disables that project, never guesses on).
        Any other scalar (``enabled: banana``) fails safe to ``False`` - the
        dangerous direction for an autonomous-dispatch opt-in is false-enabled.
        """
        # Imported at call time: fno.config imports this module while it is
        # still initializing, and the truth table is defined later in it.
        from fno.config import _coerce_affirmative

        if isinstance(v, bool):
            return v
        if isinstance(v, dict):
            return {str(k): _coerce_affirmative(val, False) for k, val in v.items()}
        if isinstance(v, str):
            # A quoted boolean is honored here but reads identically to a bare
            # one in a readout, and only one is a boolean (x-338c).
            warnings.warn(
                f"config active_backlog.enabled is the quoted string {v!r}; "
                "write a bare true/false so a readout cannot mislead",
                stacklevel=2,
            )
        return _coerce_affirmative(v, False)

    @field_validator("interval", mode="before")
    @classmethod
    def _coerce_interval(cls, v: object) -> object:
        """Drop a non-string/non-int interval to the default; never raise.

        Whether the value parses to a *positive* duration is decided by the
        consumer accessors (:meth:`interval_seconds` / :meth:`is_enabled_for`),
        which fail closed on a bad interval rather than raising (Boundaries).
        """
        if isinstance(v, bool):
            return "5m"
        if isinstance(v, str):
            return v
        if isinstance(v, (int, float)):
            return f"{int(v)}s"
        return "5m"

    @field_validator("failure_limit", mode="before")
    @classmethod
    def _coerce_failure_limit(cls, v: object) -> object:
        """Drop a non-positive-int failure_limit to the default (3); never raise."""
        if isinstance(v, bool):
            return 3
        if isinstance(v, int) and v >= 1:
            return v
        if isinstance(v, str):
            try:
                n = int(v.strip())
            except ValueError:
                return 3
            return n if n >= 1 else 3
        return 3

    @field_validator("max_concurrent", mode="before")
    @classmethod
    def _coerce_max_concurrent(cls, v: object) -> object:
        """Drop a non-positive-int max_concurrent to the default (1); never raise."""
        if isinstance(v, bool):
            return 1
        if isinstance(v, int) and v >= 1:
            return v
        if isinstance(v, str):
            try:
                n = int(v.strip())
            except ValueError:
                return 1
            return n if n >= 1 else 1
        return 1

    def interval_seconds(self) -> Optional[int]:
        """Parsed poll-floor in seconds, or ``None`` if the interval is invalid."""
        return _parse_duration_to_seconds(self.interval)

    def is_enabled_for(self, project: Optional[str]) -> bool:
        """Whether the daemon should drain ``project``.

        Fail-closed: an invalid interval disables the feature entirely (no
        0-sleep hot loop). ``enabled: true`` enables every project; a
        per-project map enables only its truthy keys.
        """
        if self.interval_seconds() is None:
            return False
        en = self.enabled
        if isinstance(en, dict):
            if project is None:
                return False
            return bool(en.get(project, False))
        return bool(en)

    def enabled_projects(self) -> list[str]:
        """The explicitly-enabled project names (per-project map mode only)."""
        if isinstance(self.enabled, dict):
            return [p for p, on in self.enabled.items() if on]
        return []

    def any_enabled(self) -> bool:
        """Whether the feature is on for >=1 project (and the interval is valid)."""
        if self.interval_seconds() is None:
            return False
        if isinstance(self.enabled, dict):
            return any(self.enabled.values())
        return bool(self.enabled)


