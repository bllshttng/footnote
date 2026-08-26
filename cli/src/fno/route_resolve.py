"""Dispatch-time model resolution: the pareto router's read side.

A task or node may pin an exact model (``model``) or a work-difficulty band
(``difficulty: high|medium|low``). At dispatch this resolves a band to the
cheapest reachable mapped model that clears the band's floor, per the cached
benchmark snapshot -- and degrades, never blocks: an empty band falls through to
a lower band and finally to the provider default, recording the fallback chain so
the choice is auditable. It NEVER touches the network; a missing snapshot uses the
static table in :mod:`fno.adapters.providers.benchmarks`.

Full precedence (Locked Decision 1):
    dispatch --model > task ``model:`` > task ``difficulty:`` > plan ``model:`` >
    plan ``difficulty:`` > provider default (``--role`` routing / provider-rotation
    combos live downstream and only fire when nothing above resolves a model).
"""
from __future__ import annotations

from typing import Optional

from fno.adapters.providers import benchmarks as bm

# A benchmark row's ``coding_percentile`` decides its band; a tier is a MINIMUM,
# so a model "clears" it by landing at that floor or above.
_BAND_FLOOR = {"low": 50, "medium": 70, "high": 90}

# Static fallback order per tier (no snapshot -> no percentiles to compare):
# requested band first, then higher bands (they clear the minimum), then lower
# bands as a last-resort degrade before the provider default.
_STATIC_FALLTHROUGH = {
    "high": ["high", "medium", "low"],
    "medium": ["medium", "high", "low"],
    "low": ["low", "medium", "high"],
}

_GRID_CANDIDATES = {
    "high": ["claude-opus-4-8", "gpt-5.5"],
    "medium": ["claude-sonnet-5", "gpt-5.4", "glm-5.3"],
    "low": ["glm-4.7", "claude-haiku-4-5"],
}


def resolve_grid(
    difficulty: Optional[str],
    priority: Optional[str],
    capacity: Optional[dict[str, object]],
) -> tuple[Optional[dict[str, str]], list[str]]:
    """Join intrinsic difficulty and priority with a live capacity snapshot.

    The grid is a default route only. ``capacity`` is deliberately supplied by
    the runtime seam so this pure resolver never reads accounts or the network.
    Unknown capacity is inert and returns no candidate.
    """
    band = (difficulty or "").strip().lower()
    prio = (priority or "p2").strip().lower()
    chain = [f"grid difficulty({band}) priority({prio})"]
    if band not in _GRID_CANDIDATES or prio not in {"p0", "p1", "p2", "p3"}:
        chain.append("grid=invalid-input")
        return None, chain
    if not capacity:
        chain.append("grid=unknown-capacity")
        return None, chain

    # p0 gets the high-urgency candidate order, while p3 intentionally prefers
    # the low-cost band. p1/p2 preserve the filer's intrinsic difficulty.
    candidate_band = "high" if prio == "p0" else "low" if prio == "p3" else band
    statuses = {str(k): str(v).lower() for k, v in capacity.items()}
    saw_unknown = False
    for model in _GRID_CANDIDATES[candidate_band]:
        reachable = bm.reachable(model)
        if reachable is None:
            continue
        harness = reachable[0]
        state = statuses.get(harness)
        if state in (None, "unknown", "stale", "none"):
            saw_unknown = True
            continue
        if state in ("exhausted", "blocked"):
            continue
        if state in ("ok", "low", "available"):
            chain.append(f"grid candidate {harness}/{model} capacity={state}")
            return {"harness": harness, "model": model}, chain
    if saw_unknown:
        chain.append("grid=unknown-capacity")
    else:
        chain.append("grid=no-available-candidate")
    return None, chain


def runtime_capacity(
    providers: tuple[str, ...] = ("claude", "codex", "gemini", "opencode"),
) -> dict[str, str]:
    """Read cached provider headroom without probing or consulting accounts."""
    try:
        from fno.adapters.providers.runtime_state import headroom

        return {provider: headroom(provider).state.value for provider in providers}
    except Exception:  # noqa: BLE001 - unknown capacity keeps the grid inert
        return {}


def _harness_ok(name: str, provider: Optional[str]) -> bool:
    """True if ``name`` maps to a harness AND (``provider`` is None or matches it).

    ``provider=None`` = no scoping (any reachable harness); a concrete provider
    keeps only same-harness candidates so a tier never picks a cross-harness model
    (Locked Decision 1/2). An unknown provider matches nothing -> all bands empty
    -> provider default.
    """
    reach = bm.reachable(name)
    return reach is not None and (provider is None or reach[0] == provider)


def _reachable_models_with_pct(
    snapshot: dict, provider: Optional[str] = None
) -> list[tuple[str, float]]:
    """(name, coding_percentile) for snapshot rows reachable by ``provider``."""
    out: list[tuple[str, float]] = []
    for row in snapshot.get("models", []):
        if not isinstance(row, dict):
            continue
        name = row.get("name")
        pct = row.get("coding_percentile")
        if name and _harness_ok(str(name), provider) and pct is not None:
            try:
                out.append((str(name), float(pct)))
            except (TypeError, ValueError):
                continue
    return out


def resolve_tier(
    tier: Optional[str],
    *,
    snapshot: Optional[dict] = None,
    provider: Optional[str] = None,
) -> tuple[Optional[str], list[str]]:
    """Resolve a tier to a concrete reachable model. Returns ``(model, chain)``.

    ``provider`` scopes the candidate set to one harness (Locked Decision 1): a
    band left empty by the filter falls through the remaining bands within the
    same harness, then to None (provider default) -- never a foreign-harness
    model. ``provider=None`` is unscoped (the dispatch seam resolves the default
    provider before calling; direct/primitive callers get the old any-harness
    behavior). ``model`` is None when nothing resolves (the caller uses the
    provider default). ``chain`` records each step so the receipt shows how the
    choice (or fallback) was reached. Never raises, never hits the network.
    """
    band = (tier or "").strip().lower()
    chain = [f"tier({band})"]
    if provider:
        chain.append(f"provider({provider})")
    if band not in _BAND_FLOOR:
        chain.append("unknown-tier -> provider default")
        return None, chain

    if snapshot is None:
        snapshot = bm.load_snapshot()

    if snapshot and snapshot.get("models"):
        models = _reachable_models_with_pct(snapshot, provider)
        floor = _BAND_FLOOR[band]
        clearing = [(n, p) for (n, p) in models if p >= floor]
        if clearing:
            # cheapest that clears the floor = lowest percentile (a
            # cheapest-that-clears proxy: the snapshot carries no cost column
            # yet; swap in real cost when it does).
            name = min(clearing, key=lambda t: (t[1], t[0]))[0]
            chain.append(f"snapshot band(>={floor}) -> {name}")
            return name, chain
        below = [(n, p) for (n, p) in models if p < floor]
        if below:
            name = max(below, key=lambda t: (t[1], t[0]))[0]
            chain.append(f"snapshot band(>={floor}) empty -> degrade -> {name}")
            return name, chain
        chain.append("snapshot has no reachable model -> provider default")
        return None, chain

    # No snapshot: walk the curated static bands in fall-through order.
    chain.append("no snapshot -> static table")
    for cand_band in _STATIC_FALLTHROUGH[band]:
        for name in bm.STATIC_TIERS.get(cand_band, []):
            if _harness_ok(name, provider):
                chain.append(f"static {cand_band} -> {name}")
                return name, chain
    chain.append("static table exhausted -> provider default")
    return None, chain


def resolve_dispatch_model(
    *,
    explicit: Optional[str] = None,
    task_model: Optional[str] = None,
    task_difficulty: Optional[str] = None,
    plan_model: Optional[str] = None,
    plan_difficulty: Optional[str] = None,
    snapshot: Optional[dict] = None,
    provider: Optional[str] = None,
) -> tuple[Optional[str], str, list[str]]:
    """Apply the full precedence chain. Returns ``(model, decision_source, chain)``.

    ``model`` is None only when everything falls through to the provider default.
    ``decision_source`` is the receipt vocabulary
    (``explicit`` / ``task-pin`` / ``task-difficulty(<band>)`` / ``plan-default`` /
    ``plan-difficulty(<band>)`` / ``provider-default``). ``provider`` scopes band
    resolution to one harness; pins (``explicit`` / ``task_model`` / ``plan_model``)
    bypass the filter -- operator authority outranks routing (Locked Decision 4).
    """
    if explicit:
        return explicit, "explicit", ["explicit"]
    if task_model:
        return task_model, "task-pin", ["task-pin"]
    if task_difficulty:
        model, chain = resolve_tier(task_difficulty, snapshot=snapshot, provider=provider)
        return model, f"task-difficulty({task_difficulty.strip().lower()})", chain
    if plan_model:
        return plan_model, "plan-default", ["plan-default"]
    if plan_difficulty:
        model, chain = resolve_tier(plan_difficulty, snapshot=snapshot, provider=provider)
        return model, f"plan-difficulty({plan_difficulty.strip().lower()})", chain
    return None, "provider-default", ["provider-default"]


def node_model(
    node: dict,
    *,
    explicit: Optional[str] = None,
    snapshot: Optional[dict] = None,
    provider: Optional[str] = None,
    resolve_difficulty: bool = True,
) -> Optional[str]:
    """Concrete ``--model`` for a node/task at the spawn seam, or None for default.

    Reads the node's own ``model`` pin and ``difficulty`` band and applies
    the precedence with an optional dispatch-time ``explicit`` override.
    ``provider`` scopes band resolution to the spawn harness so a band never
    yields a cross-harness ``<provider> --model <foreign>`` pick. When
    ``provider`` is
    None it defaults to ``claude`` -- the bg substrate's own spawn default (see
    ``advance._spawn_worker``: ``(provider or "").strip() or "claude"``), NOT the
    ambient/invoking harness. A bg worker is always claude regardless of which
    harness dispatched it, so scoping by the invoking harness would resolve a
    codex model for a claude spawn (Locked Decision 3 intent: scope the incident
    bg-default lane, which is claude). Strictly non-fatal: any resolution error
    degrades to the explicit override or the node's raw ``model`` pin so a routing
    hiccup never breaks a spawn (Locked Decision 10).
    """
    try:
        model, _source, _chain = resolve_dispatch_model(
            explicit=explicit,
            task_model=node.get("model"),
            task_difficulty=node.get("difficulty") if resolve_difficulty else None,
            snapshot=snapshot,
            provider=provider or "claude",
        )
        return model
    except Exception:  # noqa: BLE001 - routing degrades, never blocks a spawn
        return explicit if explicit is not None else node.get("model")
