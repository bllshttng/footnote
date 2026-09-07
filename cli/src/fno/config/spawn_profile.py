"""The spawn-defaults and per-verb profile blocks (``config.agents.*``).

Their own module because ``config/__init__.py`` sits over the file budget and
may only shrink, and the slot vocabulary belongs where the question it
answers lives: which lane does this dispatch ride. Field semantics live in
docs/architecture/role-based-model-routing.md.
"""
from __future__ import annotations

from typing import Any

from pydantic import BaseModel, ConfigDict, Field


class SpawnDefaultsBlock(BaseModel):
    """Default spawn routing (nested under 'config.agents.defaults').

    The bottom-most operator rung of the spawn precedence chain: an explicit
    CLI flag > these defaults > the built-in. Empty string = unset; an unset
    field falls through to the built-in. These defaults reach every spawn that
    has not pinned a field, including autonomous dispatch; an explicit flag
    always wins. No value validation here: config stays a leaf module (x-7fdd,
    no import from agents/harnesses at load time) - provider is checked
    against the known set at the spawn seam, effort against the per-provider
    surface.
    """

    model_config = ConfigDict(extra="ignore")

    provider: str = ""
    model: str = ""
    effort: str = ""
    # Config-sourced values degrade open with a warning on provider
    # incompatibility at the spawn seam; an explicit flag stays fail-closed.
    substrate: str = ""
    permission_mode: str = ""
    # route/account sit BESIDE the legacy provider field (ruling 4): provider
    # keeps meaning harness and is allowlisted as a harness literal, so a
    # stage that needed to say zai had no field. route carries vendor/model as
    # vendor/model (forwarded as --route, fail-closed downstream); account
    # forwards --account. The names carry no axis word, so the four-axis guard
    # never reads them as bindings.
    route: str = ""
    account: str = ""


class SpawnProfileBlock(SpawnDefaultsBlock):
    """Per-verb overlay plus its strict ordered delivery-lane vocabulary."""

    pane_group: str = ""
    # Keep lanes raw so a malformed routing list does not make every config
    # read fail; the spawn seam validates and refuses before launching anything.
    lanes: Any = Field(default_factory=list)
    # The declared terminal when EVERY lane is skipped: refuse (default) stops
    # the spawn; degrade lets the profile scalars answer; queue emits the
    # typed exit-78 capacity refusal. An out-of-enum value refuses at the
    # spawn seam by name rather than coercing to a terminal nobody named.
    on_exhausted: str = "refuse"
