"""The Claude tier aliases and the tier_models config validation.

One list, restated nowhere: runtime derives ``MODEL_ENV_KEYS`` from
``TIER_ALIASES`` and the config layer validates ``tier_models`` keys
against it. Runtime importing this module is the legal direction.
"""
from typing import Optional

#: Names Claude Code resolves through ``ANTHROPIC_DEFAULT_<TIER>_MODEL``.
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
