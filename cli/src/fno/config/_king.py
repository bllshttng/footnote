"""The king loop's config block.

Lives in its own module because ``config/__init__.py`` is over the file
budget and shrink-only; a new setting never grows it.
"""
from pydantic import BaseModel, ConfigDict, ValidationInfo, field_validator

import re

#: The texts a reign self-injects as native commands, module-level so the
#: fail-safe validators return the same default the field was born with.
KING_CHECKIN_TEXT = (
    "reign check-in. Run fno agents king checkin: it gathers the check-in "
    "readings, prints them, diffs the last beat, and journals reign_checkin. "
    "Then act on the printout per the reign skill. When nothing changed and "
    "coverage is full, print 'no change' and stop. This beat is a heartbeat. "
    "The heartbeat confirms that the settled-PR monitor still runs. If it does "
    "not, the heartbeat re-arms it."
)
KING_GOAL_TEXT = (
    "reign goal. When every node in the crown scope reads done or "
    "superseded, the goal is met. An open operator question blocks "
    "completion. An empty actionable queue is a quiet beat, never a "
    "finish line. A stand-down order from the operator ends the reign. "
    "Until then keep reigning. Never /goal clear on NoProgress."
)


class KingBlock(BaseModel):
    """The king loop (config.king). Field detail is in registry.py's Meta
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
    # Directive point 5, enforced mechanically by the king-delegation-guard
    # hook: what a crowned court session may write. `refuse` (default) denies
    # source authorship; `warn` prints the refusal on stderr and allows, so an
    # install adopts the guard without a surprise refusal mid-reign; `off`
    # silences it. An unknown value degrades to `refuse`, the deliberate
    # default.
    implementation_guard: str = "refuse"
    write_roots: list[str] = []
    # The monitor and stop hook are the beat; the cron proves they are alive.
    checkin_interval: str = "4h"
    checkin_text: str = KING_CHECKIN_TEXT
    goal_text: str = KING_GOAL_TEXT
    # The verdict's compaction bound; default 3 because one crown
    # produced two compaction-caused retractions in one evening.
    compaction_ceiling: int = 3

    @field_validator("checkin_interval", mode="before")
    @classmethod
    def _coerce_checkin_interval(cls, v: object) -> str:
        """Fail-safe to 4h on anything but ``<digits>[smhd]``.

        A bad value degrades, never raises: the interval arms a self-injected
        /loop, and a typo there must not kill a reign at config load.
        """
        if isinstance(v, str) and re.fullmatch(r"\d+[smhd]?", v.strip()):
            return v.strip()
        return "4h"

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

    @field_validator("checkin_text", "goal_text", mode="before")
    @classmethod
    def _coerce_reign_text(cls, v: object, info: ValidationInfo) -> str:
        """Fail-safe to the block default on a non-string or blank value."""
        if isinstance(v, str) and v.strip():
            return v
        return KING_CHECKIN_TEXT if info.field_name == "checkin_text" else KING_GOAL_TEXT
