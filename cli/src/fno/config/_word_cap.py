"""Per-surface masked-word caps (`config.style.word_cap`).

Own module so the config root stays shrink-only at its 5,000-line budget:
the ask surface (law d-59af3235) joined here and the block moved with it.
"""

from pydantic import BaseModel, ConfigDict, field_validator


class WordCapBlock(BaseModel):
    """Per-surface masked-word caps for style rule 7.

    One number per surface that is read MID-TURN. The surface set itself lives
    in `fno.style.CAPPED_SURFACES` and is not configurable: a project may move a
    number here, and may never cap a surface the checker says is uncapped.

    Bounds come from a validator, not from `Field(80, ge=1)`. A positional
    default plus a constraint makes mypy resolve this class's `default_factory`
    use as `Callable[[], Never]`, and `fno.config` is held to mypy strict.
    `MaintainBlock` is the same shape for the same reason.
    """

    model_config = ConfigDict(extra="ignore")

    mail: int = 80
    encounter: int = 80
    ask: int = 40

    @field_validator("mail", "encounter", "ask")
    @classmethod
    def cap_is_positive(cls, v: int) -> int:
        """A cap below one refuses every message, including its own refusal."""
        if v < 1:
            raise ValueError("config.style.word_cap.<surface> must be >= 1")
        return v
