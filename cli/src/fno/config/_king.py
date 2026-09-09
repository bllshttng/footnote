"""The king loop's config block.

Lives in its own module because ``config/__init__.py`` is over the file
budget and shrink-only; a new setting never grows it.
"""
from pydantic import BaseModel, ConfigDict, ValidationInfo, field_validator

import re

#: The texts a reign self-injects as native commands, module-level so the
#: fail-safe validators return the same default the field was born with.
KING_CHECKIN_TEXT = (
    "reign check-in. Run the check-in body of the reign skill "
    "(skills/reign/SKILL.md). Journal reign_checkin. When nothing changed "
    "since the last check-in, print 'no change' and stop."
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
    # The reign skill carries these defaults verbatim; the keys are the one
    # place an operator edits them.
    checkin_interval: str = "30m"
    checkin_text: str = KING_CHECKIN_TEXT
    goal_text: str = KING_GOAL_TEXT

    @field_validator("checkin_interval", mode="before")
    @classmethod
    def _coerce_checkin_interval(cls, v: object) -> str:
        """Fail-safe to 30m on anything but ``<digits>[smhd]``.

        A bad value degrades, never raises: the interval arms a self-injected
        /loop, and a typo there must not kill a reign at config load.
        """
        if isinstance(v, str) and re.fullmatch(r"\d+[smhd]?", v.strip()):
            return v.strip()
        return "30m"

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

    @field_validator("checkin_text", "goal_text", mode="before")
    @classmethod
    def _coerce_reign_text(cls, v: object, info: ValidationInfo) -> str:
        """Fail-safe to the block default on a non-string or blank value."""
        if isinstance(v, str) and v.strip():
            return v
        return KING_CHECKIN_TEXT if info.field_name == "checkin_text" else KING_GOAL_TEXT
