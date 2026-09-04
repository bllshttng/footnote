"""The heal drive loop's config block.

Lives in its own module because ``config/__init__.py`` is over the file
budget and shrink-only; a new setting never grows it.
"""
from pydantic import BaseModel, ConfigDict


class AutoHealBlock(BaseModel):
    """The pr-watch tick's heal phase (nested under 'auto_heal').

    ``enabled`` arms the phase that runs the CI heal drive loop
    (``fno do pr heal --all --apply`` in Rust) once per tick. Default false
    until the loop is measured on real PRs: an arm that pushes to branches
    is opt-in by design, like every other acting phase on the tick.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
