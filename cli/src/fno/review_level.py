"""Review-level resolution: level -> (band, effort, model), one seam.

`/fno:review <level>` is one verb on every harness, so the level must mean the
same thing everywhere: a level selects a BAND (which model family runs) and an
EFFORT (how hard it thinks), through the same provider-scoped resolver dispatch
already uses. Effort travels beside the model, never folded into it - the
five-axis vocabulary (harness, provider, model, effort, account) is why `max`
and `high` can share a model and still be different reviews.

`level_source` records how the level was chosen: `explicit` (a token on the
invocation), `diff-sized` (level_for_diff over the branch diff), `fallback`
(no measurable diff; the shipped default, named). There is no `last_used` and
nothing persists a typed level: a bare verb silently reusing the last level
anyone typed is the upstream hazard this refuses.
"""
from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from fno.route_resolve import resolve_tier
from fno.review_capability import diff_review_level

#: `static <band> -> <model>` pick entries in a resolution chain, including
#: the provider-family override form (`provider family(p) static <band> -> `).
_STATIC_PICK_RE = re.compile(r"static ([a-z]+) -> ")

#: Four levels, four bands. `xhigh` shares the high band with max EFFORT: the
#: band names the model family, the effort axis is what xhigh buys. `low`
#: carries high effort on purpose - a small model thinking hard, not briefly.
LEVEL_TO_BAND_EFFORT: dict[str, tuple[str, str]] = {
    "low": ("low", "high"),
    "medium": ("medium", "high"),
    "high": ("high", "high"),
    "xhigh": ("high", "max"),
    "max": ("max", "max"),
}

#: The shipped default when no level is explicit and no diff is measurable.
#: Named in the resolution record so a fallback never reads as a choice.
FALLBACK_LEVEL = "medium"

#: Route provider -> the harness whose model surface serves it for review
#: routing (reachability rows name a harness; the route stamp names a
#: provider). Unknown or unmapped providers resolve UNSCOPED, which is the
#: resolver's any-harness behavior, never a refusal.
_PROVIDER_HARNESS: dict[str, Optional[str]] = {
    "zai": "claude",
    "anthropic": "claude",
    "openai": "codex",
    "opencode": "opencode",
}

#: Providers that ride a SHARED harness but bill their own model families.
#: The harness-scoped resolver cannot see the provider axis (zai and
#: anthropic both reach models through the claude harness), so a routed
#: session's own family is a PREFERENCE applied over the band's candidates -
#: and, for a band the family cannot serve at all, the reason the pick walks
#: the fall-through order inside the family. The chain records the override.
_PROVIDER_MODEL_FAMILIES: dict[str, tuple[str, ...]] = {
    "zai": ("glm-",),
}


@dataclass(frozen=True)
class ReviewLevelResolution:
    """One resolved review invocation record."""

    level: str
    level_source: str  # explicit | diff-sized | fallback
    band: str
    effort: str
    model: Optional[str]
    provider: str
    chain: tuple[str, ...] = field(default_factory=tuple)
    #: The provider has no distinct max model and the max request was served by
    #: the high model at max effort. Recorded, never presented silently.
    degraded_max: bool = False

    def as_dict(self) -> dict[str, object]:
        return {
            "level": self.level,
            "level_source": self.level_source,
            "band": self.band,
            "effort": self.effort,
            "model": self.model,
            "provider": self.provider,
            "chain": list(self.chain),
            "degraded_max": self.degraded_max,
        }


def _session_provider() -> str:
    """The route stamp, read verbatim; never inferred from an endpoint."""
    return (os.environ.get("FNO_ROUTE_PROVIDER") or "").strip() or "unknown"


def _degraded_max(band: str, model: Optional[str], chain: tuple[str, ...]) -> bool:
    """Whether a max request was served by a non-max band.

    The resolver's chain names the band that served the pick (`static <band>
    -> <model>` on the static path, including the provider-family override
    entry). A max request answered by any lower band - or, on the snapshot
    path, by a model below the max floor - is a degraded max: the effort axis
    still separates it from `high`, but the model does not, and the record
    must say so.
    """
    if band != "max" or model is None:
        return False
    for entry in reversed(chain):
        match = _STATIC_PICK_RE.search(entry)
        if match:
            return match.group(1) != "max"
        if entry.startswith("snapshot band("):
            return "degrade ->" in entry
    return False


def _prefer_provider_family(
    route_provider: str,
    band: str,
    model: Optional[str],
    chain: list[str],
) -> Optional[str]:
    """Override a shared-harness pick with the routed provider's own family.

    Returns the preferred model when the band (or its fall-through order)
    holds one the provider's family serves on the same harness, else None to
    keep the resolver's pick. Scoped to providers with a declared family: an
    anthropic-routed session on the claude harness has no override, because
    every claude-harness row IS its family.
    """
    families = _PROVIDER_MODEL_FAMILIES.get(route_provider)
    if not families or (model is not None and model.startswith(families)):
        return None
    harness = _PROVIDER_HARNESS.get(route_provider)
    from fno.adapters.providers import benchmarks as bm
    from fno.route_resolve import _STATIC_FALLTHROUGH

    for cand_band in _STATIC_FALLTHROUGH.get(band, [band]):
        for name in bm.STATIC_TIERS.get(cand_band, []):
            reach = bm.reachable(name)
            if reach and reach[0] == harness and name.startswith(families):
                chain.append(f"provider family({route_provider}) static {cand_band} -> {name}")
                return name
    return None


def resolve_review_level(
    level_or_none: Optional[str],
    *,
    provider: Optional[str] = None,
    project_root: Optional[Path] = None,
) -> ReviewLevelResolution:
    """Resolve a review level to the full routing record. Never raises.

    Precedence: an explicit level token, then a level sized from the branch
    diff, then the named fallback. The band and effort come from the map, the
    model from the provider-scoped tier resolver (degrade, never block), and
    the whole chain is carried so the receipt shows how the pick was reached.
    """
    if level_or_none is not None:
        level = level_or_none.strip().lower()
        source = "explicit"
    else:
        sized = diff_review_level(project_root)
        if sized is not None:
            level = sized
            source = "diff-sized"
        else:
            level = FALLBACK_LEVEL
            source = "fallback"
    level = level if level in LEVEL_TO_BAND_EFFORT else FALLBACK_LEVEL
    band, effort = LEVEL_TO_BAND_EFFORT[level]

    route_provider = (provider if provider is not None else _session_provider()) or "unknown"
    model, chain = resolve_tier(
        band, provider=_PROVIDER_HARNESS.get(route_provider)
    )
    chain = list(chain)
    preferred = _prefer_provider_family(route_provider, band, model, chain)
    if preferred is not None:
        model = preferred
    return ReviewLevelResolution(
        level=level,
        level_source=source,
        band=band,
        effort=effort,
        model=model,
        provider=route_provider,
        chain=tuple(chain),
        degraded_max=_degraded_max(band, model, tuple(chain)),
    )
