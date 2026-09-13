"""The Claude tier aliases and the tier_models config validation.

One list, restated nowhere: ``model_routing`` derives ``MODEL_ENV_KEYS``
(and the scrub floor through it) from ``TIER_ALIASES``, and the config
layer validates ``ModelProvider.tier_models`` keys against it, so a new
tier lands everywhere at once. This module is import-only data and lives
in the config layer; runtime importing it is the legal direction (the
reverse edge is what check-company-boundaries refuses).
"""
from typing import Optional

#: The names Claude Code resolves through ``ANTHROPIC_DEFAULT_<TIER>_MODEL``.
#: ``fable`` is a live alias (``fno agents spawn --model fable``); omitting it
#: left the fable tier of a routed worker resolving at Anthropic while every
#: other tier ran on the secondary provider.
TIER_ALIASES = ("opus", "sonnet", "haiku", "fable")


def validate_tier_models(v: Optional[dict[str, str]]) -> Optional[dict[str, str]]:
    """Config-load validation for ``ModelProvider.tier_models``: a key outside
    the aliases, or a tier naming no model, is a load-time refusal naming the
    bad key and the legal set. There is no check that the provider SERVES the
    named model - a wrong id fails at the endpoint, same as a bad ``--model``."""
    if not v:
        return v
    cleaned = {str(k).strip().lower(): str(m).strip() for k, m in v.items()}
    bad = sorted(set(cleaned) - set(TIER_ALIASES))
    if bad:
        raise ValueError(
            f"tier_models keys {bad} are not Claude tier aliases; "
            f"legal keys: {', '.join(TIER_ALIASES)}"
        )
    empty = sorted(k for k, m in cleaned.items() if not m)
    if empty:
        raise ValueError(f"tier_models[{empty}] names no model")
    return cleaned
