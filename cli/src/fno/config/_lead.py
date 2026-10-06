"""The lead loop's config block.

Lives in its own module because ``config/__init__.py`` is over the file
budget and shrink-only; a new setting never grows it.
"""
from pydantic import BaseModel, ConfigDict, ValidationInfo, field_validator

import re

#: The texts a term self-injects as native commands, module-level so the
#: fail-safe validators return the same default the field was born with.
LEAD_CHECKIN_TEXT = (
    "lead check-in. Run fno agents org checkin: it gathers the check-in "
    "readings, prints them, diffs the last beat, and journals lead_checkin. "
    "Then act on the printout per the term skill. When nothing changed and "
    "coverage is full, print 'no change' and stop. This beat is a heartbeat. "
)


class LeadBlock(BaseModel):
    """The lead loop (config.lead). Field detail is in registry.py's Meta
    text, surfaced by `fno config schema`.

    ``RunAtLoad`` is false for the pr-watcher LaunchAgent by design, so a
    machine that never ran ``launchctl load`` has no waker; ``wake_enabled``
    does not change that.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    autonomous_merge: bool = False
    wake_enabled: bool = False
    wake_ceiling: int = 32
    wake_debounce_seconds: int = 900
    wake_backstop_seconds: int = 1800
    blocked_child_grace_minutes: int = 30
    # Directive point 5, enforced mechanically by the lead-delegation-guard
    # hook: what a promoted team session may write. `refuse` (default) denies
    # source authorship; `warn` prints the refusal on stderr and allows, so an
    # install adopts the guard without a surprise refusal mid-term; `off`
    # silences it. An unknown value degrades to `refuse`, the deliberate
    # default.
    implementation_guard: str = "refuse"
    write_roots: list[str] = []
    checkin_interval: str = "55m"
    checkin_text: str = LEAD_CHECKIN_TEXT

    @field_validator("checkin_interval", mode="before")
    @classmethod
    def _coerce_checkin_interval(cls, v: object) -> str:
        """Fail-safe to 55m on anything but ``<digits>[smhd]``.

        A bad value degrades, never raises: the interval arms a self-injected
        /loop, and a typo there must not kill a term at config load.
        """
        if isinstance(v, str) and re.fullmatch(r"\d+[smhd]?", v.strip()):
            return v.strip()
        return "55m"

    @field_validator("implementation_guard", mode="before")
    @classmethod
    def _coerce_implementation_guard(cls, v: object) -> str:
        """Degrade an unknown value to ``refuse``.

        A typo must not silently disarm the guard (that would be `off` by
        accident); the strict reading is the deliberate default.
        """
        if isinstance(v, str) and v.strip() in ("refuse", "warn", "off"):
            return v.strip()
        return "refuse"

    @field_validator("write_roots", mode="before")
    @classmethod
    def _coerce_write_roots(cls, v: object) -> list[str]:
        """A bare string is one root; blanks and non-strings drop, never raise."""
        if isinstance(v, str):
            v = [v]
        if not isinstance(v, list):
            return []
        return [s.strip() for s in v if isinstance(s, str) and s.strip()]

    @field_validator("checkin_text", mode="before")
    @classmethod
    def _coerce_term_text(cls, v: object, info: ValidationInfo) -> str:
        """Fail-safe to the block default on a non-string or blank value."""
        if isinstance(v, str) and v.strip():
            return v
        return LEAD_CHECKIN_TEXT
