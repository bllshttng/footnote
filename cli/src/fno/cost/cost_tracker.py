#!/usr/bin/env python3
"""Shared pricing and cost calculation for fno token tools.

Single source of truth for Anthropic model pricing; the table and its
sources live in pricing.yaml beside this module.

Usage:
    from fno.cost.cost_tracker import model_tier, calculate_cost, estimate_cache_miss_cost

    # Shell delegation target (used by scripts/metrics/cost-tracker.sh):
    python3 -m fno.cost.cost_tracker estimate <model> <input_tokens> <output_tokens> \
        [cache_read_tokens] [cache_create_tokens]
"""

import re
import sys
from pathlib import Path

# Prices per million tokens; the table itself lives in pricing.yaml beside
# this module.
import yaml as _yaml

PRICING: dict = _yaml.safe_load(
    (Path(__file__).resolve().parent / "pricing.yaml").read_text(encoding="utf-8")
)

# Default for unknown models
DEFAULT_TIER = "sonnet"

# Optimistic default for unknown / future opus versions (>= 4.5 or
# unparseable). The previous pessimistic fallback to opus-4.0 pricing
# ($15/$75) produced two 3x inflation incidents when 4.7 and 4.8 shipped
# before the table was updated (see backfill-opus47-costs.py and
# backfill-cost-recompute.py). Every future opus is >= 4.5; defaulting to
# modern pricing degrades to a small error only if Anthropic raises prices,
# which `fno doctor --cost-check` catches.
LATEST_MODERN_OPUS_TIER = "opus-5.0"

# First version number after "opus" wins, so context-size suffixes like the
# live `claude-opus-4-8[1m]` parse as (4, 8), never (1, m). The minor is
# optional because Opus 5 ships minorless (`claude-opus-5`). The `(?![\d])`
# after the major is what keeps contiguous date digits
# (claude-3-opus-20240229) from parsing as a version at all: a real major is
# 1-2 digits and is never followed by another digit, so the match fails and
# the claude-3 guard downstream routes it to legacy pricing.
_OPUS_VERSION_RE = re.compile(r"opus[^0-9]*(\d{1,2})(?![\d])(?:[._-](\d+))?")

# Model IDs that triggered the unparseable-opus fallback. The stderr
# warning fires once per process per model; callers (session-cost.py
# --json) surface this set machine-visibly so drift is observable in the
# ledger, not only on swallowed hook stderr. Tests that assert warning
# behavior must clear this set in setup (it is process-global state).
FALLBACK_MODELS_SEEN: set[str] = set()


def model_tier(model_name: str, speed: str | None = None) -> str:
    """Map a model ID string + optional speed to a pricing tier key.

    Args:
        model_name: Full model ID (e.g., 'claude-opus-4-8', 'claude-sonnet-4-5-20250929')
        speed: 'fast' or None. Only affects opus-4.6.

    Returns:
        Key into PRICING dict.
    """
    name = model_name.lower()

    if "opus" in name:
        match = _OPUS_VERSION_RE.search(name)
        if match:
            major_str, minor_str = match.group(1), match.group(2)
            # Date-artifact guard: claude-opus-4-20250514 (Opus 4.0's real
            # ID) parses as minor "20250514". Real minors are 1-2 digits;
            # anything longer is a date stamp, meaning the ID carried no
            # minor at all -> treat as .0 so history is never repriced. A
            # minorless ID (claude-opus-5) lands on .0 the same way.
            minor = int(minor_str) if minor_str and len(minor_str) <= 2 else 0
            version = (int(major_str), minor)
            exact = f"opus-{version[0]}.{version[1]}"
            if version >= (4, 5):
                if exact == "opus-4.6" and speed == "fast":
                    return "opus-4.6-fast"
                if exact in PRICING:
                    return exact
                # Future version (4.9, 5.0, ...) not yet in the table.
                return LATEST_MODERN_OPUS_TIER
            # Known historical tiers are never silently repriced:
            # opus-4.0 / 4.1 really were $15/$75.
            if exact in PRICING:
                return exact
            return "opus-4.0"
        if "claude-3" in name:
            # Opus 3 was also $15/$75.
            return "opus-4.0"
        # Unparseable opus ID: optimistic default + one-time warning.
        if model_name not in FALLBACK_MODELS_SEEN:
            FALLBACK_MODELS_SEEN.add(model_name)
            print(
                f"Warning: unrecognized opus model {model_name!r}; "
                f"falling back to {LATEST_MODERN_OPUS_TIER} pricing "
                "(update PRICING in scripts/lib/cost_tracker.py)",
                file=sys.stderr,
            )
        return LATEST_MODERN_OPUS_TIER

    if "fable" in name or "mythos" in name:
        return "fable-5"

    if "sonnet" in name:
        return "sonnet"

    if "haiku" in name:
        if "4-5" in name or "4.5" in name:
            return "haiku-4.5"
        return "haiku-3.5"

    return DEFAULT_TIER


def calculate_cost(usage: dict, model: str) -> float:
    """Calculate USD cost from a usage object and model name.

    Args:
        usage: Dict with keys: input_tokens, output_tokens,
               cache_read_input_tokens, cache_creation_input_tokens.
               Also checks server_tool_use.web_search_requests.
        model: Model ID string.

    Returns:
        Cost in USD.
    """
    speed = usage.get("speed")
    tier = model_tier(model, speed)
    prices = PRICING.get(tier, PRICING[DEFAULT_TIER])

    input_tokens = usage.get("input_tokens", 0) or 0
    output_tokens = usage.get("output_tokens", 0) or 0
    cache_read = usage.get("cache_read_input_tokens", 0) or 0
    cache_create = usage.get("cache_creation_input_tokens", 0) or 0

    web_searches = 0
    server_tool_use = usage.get("server_tool_use")
    if server_tool_use:
        web_searches = server_tool_use.get("web_search_requests", 0) or 0

    return (
        (input_tokens / 1_000_000) * prices["input"]
        + (output_tokens / 1_000_000) * prices["output"]
        + (cache_read / 1_000_000) * prices["cache_read"]
        + (cache_create / 1_000_000) * prices["cache_create"]
        + web_searches * prices["web_search"]
    )


def estimate_cache_miss_cost(cache_read_tokens: int, model: str) -> float:
    """What would it cost if these cached tokens were uncached instead?

    Returns the EXTRA cost (uncached price - cached price) for the given tokens.
    """
    tier = model_tier(model)
    prices = PRICING.get(tier, PRICING[DEFAULT_TIER])
    cached_cost = (cache_read_tokens / 1_000_000) * prices["cache_read"]
    uncached_cost = (cache_read_tokens / 1_000_000) * prices["input"]
    return uncached_cost - cached_cost


def format_tokens(n: int) -> str:
    """Format token count for display."""
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}K"
    return str(n)


def format_cost(cost: float) -> str:
    """Format cost as $X.XX."""
    return f"${cost:.2f}"


def _cli_estimate(argv: list[str]) -> int:
    """`estimate <model> <input> <output> [cache_read] [cache_create]`.

    Prints decimal USD with 4 places (the shape cost-tracker.sh's bc
    pipeline produced), so shell callers keep their parsing unchanged.
    """
    if len(argv) < 3 or len(argv) > 5:
        print(
            "usage: cost_tracker.py estimate <model> <input_tokens> "
            "<output_tokens> [cache_read_tokens] [cache_create_tokens]",
            file=sys.stderr,
        )
        return 2
    model = argv[0]
    try:
        tokens = [int(arg) for arg in argv[1:]]
    except ValueError as exc:
        print(f"cost_tracker.py estimate: non-numeric token count: {exc}", file=sys.stderr)
        return 2
    tokens += [0] * (4 - len(tokens))
    usage = {
        "input_tokens": tokens[0],
        "output_tokens": tokens[1],
        "cache_read_input_tokens": tokens[2],
        "cache_creation_input_tokens": tokens[3],
    }
    print(f"{calculate_cost(usage, model):.4f}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "estimate":
        sys.exit(_cli_estimate(sys.argv[2:]))
    print(__doc__, file=sys.stderr)
    sys.exit(2)
