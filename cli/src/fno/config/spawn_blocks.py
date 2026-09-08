"""The spawn-defaults schema blocks: one base, per-verb profiles, per-harness
overlays. Re-exported from ``fno.config`` so every reader keeps its import
path; this module exists because the config hub is over the file budget and
shrink-only (check-file-budget.sh)."""

from __future__ import annotations

from typing import Any, List

from pydantic import BaseModel, ConfigDict, Field, field_validator


class HarnessOverlayBlock(BaseModel):
    """One harness's answers under a spawn-defaults block.

    The axis vocabulary rule applied to storage: a permission mode, effort or
    substrate value is a flag spelling the HARNESS defines, not a fleet policy,
    so the answer lives keyed by harness here instead of in one scalar that
    every harness inherits. Precedence: explicit flag > lane >
    ``profiles.<verb>.harness.<h>`` > ``profiles.<verb>`` >
    ``defaults.harness.<h>`` > ``defaults``. Only fields whose vocabulary the
    harness defines and fno forwards are legal here (``permission_mode``,
    ``effort``, ``substrate``) plus ``args``: an opaque argv-vector appended
    behind a ``--`` fence, referencing the harness's own bundle (``--profile``,
    ``--settings``) through the passthrough fno already carries. Ranking
    fields (``provider``, ``model``, ``route``, ``account``) are refused at
    the spawn seam: they are lane fields, never per-harness answers.

    ``extra="allow"`` is deliberate: a smuggled lane field must survive load
    so the seam can name it in its refusal instead of the typo vanishing.
    """

    model_config = ConfigDict(extra="allow")

    permission_mode: str = ""
    effort: str = ""
    substrate: str = ""
    args: List[str] = Field(default_factory=list)


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
    # Per-harness answers (x-8975): the scalars above are the base that works
    # for most; an entry here re-answers one harness whose flag vocabulary
    # differs. The spawn seam validates the harness NAME and refuses ranking
    # fields here; config stays a leaf.
    harness: dict[str, HarnessOverlayBlock] = Field(default_factory=dict)

    @field_validator("harness", mode="before")
    @classmethod
    def _coerce_harness_overlays(cls, v: object) -> object:
        """A non-mapping overlay table, or a non-mapping entry, degrades to
        empty: one typo must never brick every command at load, mirroring
        ``_coerce_profiles``."""
        if not isinstance(v, dict):
            return {}
        return {k: val for k, val in v.items() if isinstance(val, dict)}


class SpawnProfileBlock(SpawnDefaultsBlock):
    """Per-verb overlay plus its strict ordered delivery-lane vocabulary."""

    pane_group: str = ""
    # Keep lanes raw so a malformed routing list does not make every config
    # read fail; the spawn seam validates and refuses before launching anything.
    lanes: Any = Field(default_factory=list)
