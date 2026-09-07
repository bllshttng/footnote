"""The spawn-defaults and per-verb profile blocks (``config.agents.*``).

Their own module because ``config/__init__.py`` sits over the file budget and
may only shrink, and the slot vocabulary belongs where the question it
answers lives: which lane does this dispatch ride.
"""
from __future__ import annotations

from typing import Any

from pydantic import BaseModel, ConfigDict, Field


class SpawnDefaultsBlock(BaseModel):
    """Default spawn routing (nested under 'config.agents.defaults').

    The bottom-most operator rung of the spawn precedence chain: an explicit
    CLI flag > these defaults > the built-in (provider: harness-inference then
    claude). Every bare `fno agents spawn` / `/agent spawn` inherits any field
    set here, injected field-by-field at the Python dispatch seam. Empty
    string = unset; an unset field falls through to the built-in exactly as
    today.

    These defaults reach every spawn that has not pinned a field, including
    autonomous dispatch (`/target`, think dispatch, backlog advance); an explicit
    flag always wins. Autonomous dispatch pins its harness and substrate, so
    setting `provider` here cannot silently reroute the fleet's binary - but a
    `model` or `effort` set here DOES reach an autonomous worker that left it
    unpinned, which is the per-stage coordinate the stage table exists to carry.

    No value validation here: config stays a leaf module (x-7fdd, no import
    from agents/harnesses at load time). Provider is checked against the known
    set at the spawn seam; effort against the per-provider surface.
    """

    model_config = ConfigDict(extra="ignore")

    provider: str = ""
    model: str = ""
    effort: str = ""
    # substrate/permission_mode join the defaultable set (x-3d5b): config-sourced
    # values degrade open with a warning on provider incompatibility at the spawn
    # seam; an explicit flag stays fail-closed. Empty = unset, as above.
    substrate: str = ""
    permission_mode: str = ""
    # route/account sit BESIDE the legacy provider field (ruling 4): nothing is
    # renamed and the legacy field keeps meaning harness. provider could never
    # carry the full coordinate because it is allowlisted as a harness literal,
    # so a stage that needed to say zai had no field. route carries vendor/model
    # as vendor/model (position-carried, forwarded as --route) and so fails
    # closed on an unknown vendor or a missing key downstream rather than silently
    # billing the primary; account forwards --account. The names carry no axis
    # word, so the four-axis guard never reads them as bindings.
    route: str = ""
    account: str = ""


class SpawnProfileBlock(SpawnDefaultsBlock):
    """Per-verb overlay plus its strict ordered delivery-lane vocabulary."""

    pane_group: str = ""
    # Keep lanes raw so a malformed routing list does not make every config
    # read fail; the spawn seam validates and refuses before launching anything.
    lanes: Any = Field(default_factory=list)
    # The declared terminal when EVERY lane is skipped (capped vendor,
    # exhausted account, unsupported posture): refuse (default) stops the
    # spawn; degrade lets the profile scalars and agents.defaults answer;
    # queue emits a typed capacity refusal (exit 78) so a dispatcher reads it
    # as capacity, not config. An out-of-enum value refuses at the spawn seam
    # by name rather than coercing to a terminal nobody named.
    on_exhausted: str = "refuse"
