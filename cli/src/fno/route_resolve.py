"""Dispatch-time model resolution: the config-first router's read side.

The declared inventory (``config.routing.models``) is the PRIMARY routing
surface: nothing built-in is authoritative, so adding a model, a provider or a
harness is a config edit, and a stranger's install never inherits this
machine's fleet. The OpenRouter snapshot is OPTIONAL enrichment: it may supply
a percentile that derives a band for a row whose ``band`` the operator left
unset, and it can never make the grid inert. A virgin install declares no
inventory; the grid records ``grid=no-inventory-declared`` and injects
nothing, byte-identical to today's behaviour minus the silence.

Full precedence (Locked Decision 1), now per AXIS rather than per spawn: an
explicit flag or a profile field occupies the axis it names and nothing more,
so ``[agents.profiles.target] provider = "codex"`` pins the harness and the
grid still chooses model and effort within codex.
    dispatch --model > task ``model:`` > task ``difficulty:`` > plan ``model:`` >
    plan ``difficulty:`` > provider default (``--role`` routing / provider-rotation
    combos live downstream and only fire when nothing above resolves a model).
"""
from __future__ import annotations

import dataclasses
from typing import Any, Mapping, Optional, Sequence

from fno.adapters.providers import benchmarks as bm

# A benchmark row's ``coding_percentile`` decides its band; a tier is a MINIMUM,
# so a model "clears" it by landing at that floor or above. `max` is the one
# deliberate asymmetry: its floor sits above high's, and a max request takes
# the STRONGEST reachable model rather than the cheapest that clears, because
# max semantics invert the cheapest-clearing rule.
_BAND_FLOOR = {"low": 50, "medium": 70, "high": 90, "max": 95}

# Static fallback order per tier (no snapshot -> no percentiles to compare):
# requested band first, then higher bands (they clear the minimum), then lower
# bands as a last-resort degrade before the provider default. `max` never
# degrades UP (nothing sits above it) and degrades down only inside the same
# provider; a max answered by the high band is recorded as degraded, never
# presented as a max (review_level._degraded_max reads the chain for it).
_STATIC_FALLTHROUGH = {
    "max": ["max", "high", "medium", "low"],
    "high": ["high", "medium", "low"],
    "medium": ["medium", "high", "low"],
    "low": ["low", "medium", "high"],
}

# Strong end of the band vocabulary; the round-up ruling resolves absent or
# uncertain difficulty here, never to the cheap end. `max` ranks above `high`
# so the band vocabulary here is the SAME one `_BAND_FLOOR` admits: a declared
# max row must not fall through to rank -1.
_BAND_RANK = {"low": 0, "medium": 1, "high": 2, "max": 3}
_STRONG_BAND = "high"
_OBJECTIVES = ("cheapest-that-clears", "best-available", "prefer-harness")
_PLANNING_BAND = "high"

# Aggregation order for a harness's accounts: MAX over headroom. ok > low >
# unknown > exhausted. Unknown outranks exhausted because exhaustion is only
# true when EVERY account says so (M2/t2.1): one silent account never walls a
# harness another account can still serve. The MAX aggregate is the
# HARNESS-WIDE answer and is correct for a row that names no account; a row
# that names an account gets that account's own answer (:func:`row_capacity`).
_CAPACITY_RANK = {"ok": 3, "available": 3, "low": 2, "unknown": 1, "exhausted": 0, "blocked": 0}


@dataclasses.dataclass(frozen=True)
class InventoryRow:
    """One resolved inventory row. ``band`` is "" when unbanded, and an
    unbanded row is a grid candidate at every band: it ranks after the banded
    rows that clear, in declared order."""

    name: str
    harness: str
    model: str
    route: str = ""
    account: str = ""
    band: str = ""
    percentile: Optional[float] = None
    effort: str = ""
    cost_per_mtok_in: Optional[float] = None
    context: Optional[int] = None

    @property
    def rank(self) -> int:
        return _BAND_RANK.get(self.band, -1)

    def accounts(self) -> list[str]:
        """The account record id whose quota this row spends, if named.

        ``route`` deliberately contributes nothing: it names a VENDOR lane
        (``zai/glm-5.3``), and ``provider_health``/``usage`` are keyed by
        ``config.accounts.records`` id. A vendor string can never match a key,
        so folding it into the account set would add a pseudo-account whose
        permanent UNKNOWN dilutes a real account's live lock in the MAX
        aggregate - the one shape this change exists to kill.
        """
        return [self.account] if self.account else []


@dataclasses.dataclass(frozen=True)
class Inventory:
    """The resolved inventory plus its objective (the objective is config-owned).

    ``rows`` is the built-in fallback table overridden and extended by config.
    ``declared`` says whether CONFIG named any row, which ``rows`` alone can no
    longer answer now that the fallback seeds it. The grid reads ``declared``:
    a virgin install still injects nothing.
    """

    rows: dict[str, InventoryRow] = dataclasses.field(default_factory=dict)
    objective: str = _OBJECTIVES[0]
    prefer_harness: str = ""
    declared: bool = False


def _field(source: Mapping[str, Any] | object, name: str, default: Any = "") -> Any:
    if isinstance(source, Mapping):
        value = source.get(name, default)
    else:
        value = getattr(source, name, default)
    return default if value is None else value


def _band_from_percentile(pct: Optional[float]) -> str:
    if pct is None:
        return ""
    if pct >= _BAND_FLOOR["high"]:
        return "high"
    if pct >= _BAND_FLOOR["medium"]:
        return "medium"
    if pct >= _BAND_FLOOR["low"]:
        return "low"
    return ""


def inventory_from_rows(
    rows: Sequence[Mapping[str, Any] | object],
    *,
    objective: str = _OBJECTIVES[0],
    prefer_harness: str = "",
    snapshot: Optional[dict] = None,
    declared: bool = True,
) -> Inventory:
    """Fold declared rows into an :class:`Inventory`.

    Rows are keyed by ``name``; a later row of the same name overrides per
    field and the fields it did not name keep the earlier row's value (the
    merge precedent from ``model_routing._DEFAULT_PROVIDERS``). Band
    resolution per row: the row's own ``band``, else a snapshot percentile
    against ``_BAND_FLOOR``, else unbanded. A row with no band is a candidate
    at every band; it ranks after the banded rows that clear, and
    ``fno config route inventory`` labels it ``unbanded``.
    """
    folded: dict[str, dict[str, Any]] = {}
    order: list[str] = []
    for row in rows:
        name = str(_field(row, "name", "") or "").strip()
        if not name:
            continue
        if name not in folded:
            folded[name] = {}
            order.append(name)
        for key in (
            "name", "harness", "model", "route", "account", "band", "effort",
            "cost_per_mtok_in", "context",
        ):
            value = _field(row, key, None)
            if value not in (None, ""):
                folded[name][key] = value
    snap_pct: dict[str, float] = {}
    if snapshot:
        for entry in snapshot.get("models", []):
            if isinstance(entry, dict) and entry.get("name") is not None:
                pct = entry.get("coding_percentile")
                if pct is None:
                    continue
                try:
                    snap_pct[str(entry["name"])] = float(pct)
                except (TypeError, ValueError):
                    continue
    out: dict[str, InventoryRow] = {}
    for name in order:
        merged = folded[name]
        pct = snap_pct.get(name)
        band = str(merged.get("band", "") or "").strip().lower()
        if band not in _BAND_FLOOR:
            band = _band_from_percentile(pct)
        out[name] = InventoryRow(
            name=name,
            harness=str(merged.get("harness", "") or "").strip(),
            model=str(merged.get("model", "") or "").strip(),
            route=str(merged.get("route", "") or "").strip(),
            account=str(merged.get("account", "") or "").strip(),
            band=band,
            percentile=pct,
            effort=str(merged.get("effort", "") or "").strip(),
            cost_per_mtok_in=merged.get("cost_per_mtok_in"),
            context=merged.get("context"),
        )
    obj = objective if objective in _OBJECTIVES else _OBJECTIVES[0]
    return Inventory(
        rows=out, objective=obj, prefer_harness=prefer_harness or "", declared=declared
    )



def _builtin_rows() -> list[dict[str, Any]]:
    """The built-in table as inventory rows: a FALLBACK, never the authority.

    Config overrides and extends these. `inventory_from_rows` folds per name
    and per field, so a config row naming an existing model replaces only the
    fields it names, and a new name is simply added. That is the same merge
    `model_routing._DEFAULT_PROVIDERS` uses, and it is what keeps adding a
    model a config edit rather than a Python edit.

    A model the table lists in two bands (`gpt-5.6-sol` is in both `max` and
    `high`) keeps the STRONGEST one, picked by rank here rather than by the
    order rows happen to be emitted in. One row per name, so the fold has no
    same-name ordering to depend on.
    """
    from fno.adapters.providers import benchmarks as _bm

    strongest: dict[str, str] = {}
    for band, names in _bm.STATIC_TIERS.items():
        if band not in _BAND_RANK:
            continue
        for name in names:
            held = strongest.get(name)
            if held is None or _BAND_RANK[band] > _BAND_RANK[held]:
                strongest[name] = band
    rows: list[dict[str, Any]] = []
    for name in sorted(strongest):
        reach = _bm.REACHABILITY.get(name)
        if reach is None:
            continue
        rows.append(
            {
                "name": name,
                "harness": reach[0],
                "model": reach[1],
                "band": strongest[name],
            }
        )
    return rows


def resolve_inventory(
    *,
    settings: object = None,
    snapshot: Optional[dict] = None,
) -> Inventory:
    """Read the declared inventory from config (empty when nothing is declared).

    Never raises on a config problem: an unloadable config is an EMPTY
    inventory (the grid records ``no-inventory-declared``), not a dead spawn.
    """
    try:
        if settings is None:
            from fno.config import load_settings

            settings = load_settings()
        routing = getattr(settings, "routing", None)
        if snapshot is None:
            snapshot = bm.load_snapshot()
        cfg_rows = list(getattr(routing, "models", []) or [])
        return inventory_from_rows(
            _builtin_rows() + cfg_rows,
            objective=str(getattr(routing, "objective", "") or ""),
            prefer_harness=str(getattr(routing, "prefer_harness", "") or ""),
            snapshot=snapshot,
            declared=bool(cfg_rows),
        )
    except Exception:  # noqa: BLE001 - a routing read never breaks a spawn
        return Inventory()


def _order_candidates(
    candidates: list[InventoryRow], inventory: Inventory
) -> list[InventoryRow]:
    """Order candidates by the declared objective. Never lowers the band: the
    band admission already happened before this runs."""
    objective = inventory.objective
    if objective == "best-available":
        return sorted(candidates, key=lambda r: (-r.rank, -(r.percentile or -1.0), r.name))
    if objective == "prefer-harness":
        preferred = inventory.prefer_harness
        # Tier wins, harness is a tiebreaker within a tier: stable partition by
        # the preferred harness, band-descending inside each partition.
        return sorted(
            candidates,
            key=lambda r: (
                0 if r.harness == preferred else 1,
                -r.rank,
                -(r.percentile or -1.0),
                r.name,
            ),
        )
    # cheapest-that-clears: declared cost first (by cost), then the percentile
    # proxy for rows that declare none (the snapshot carries no cost column);
    # a row with neither signal is cheapest at the WEAKEST band that still
    # clears, never the strongest (that is best-available's job).
    def _cheapest_key(r: InventoryRow) -> tuple:
        if r.cost_per_mtok_in is not None:
            return (0, r.cost_per_mtok_in, r.rank, r.name)
        if r.percentile is not None:
            return (1, r.percentile, r.rank, r.name)
        return (2, 0, r.rank, r.name)

    return sorted(candidates, key=_cheapest_key)


def _candidate_supported(
    harness: str, substrate: Optional[str], permission_mode: Optional[str]
) -> bool:
    """Whether a pinned substrate / permission mode can legally ride ``harness``.

    Posture flags FILTER the candidate set (t3.2); they never cancel the
    routing decision. Mirrors the spawn parser's own gates: thread needs the
    harness's journey-proven lane (its spawn claim reads native), a mapped
    permission mode is claude's on every substrate and a non-claude harness's
    only on the pane lane. An unset substrate reads as the spawn parser's own
    default (pane), so a lone permission pin does not filter out non-claude
    rows the gate would accept. An unknown harness degrades open (kept) so
    the spawn's own gate, which names the value, stays the authority on
    refusal.
    """
    sub = (substrate or "").strip()
    if sub == "bg":
        sub = "thread"
    if sub == "thread":
        try:
            from fno.agents.harness_map import thread_seatable

            if not thread_seatable(harness):
                return False
        except Exception:  # noqa: BLE001 - unknown harness keeps the candidate
            pass
    mode = (permission_mode or "").strip()
    if mode:
        # "" (unset) is pane here for the same reason _permission_mappable
        # takes the parser's pane default: only a NON-pane substrate narrows.
        if harness != "claude" and sub not in ("", "pane"):
            return False
    return True


def _harness_installed(harness: str) -> bool:
    """Whether a harness fno can drive is named. Degrades open (True) on an
    unreadable roster so the spawn's own gate, which names the value, keeps the
    authority to refuse."""
    try:
        from fno.agents.harnesses import READABLE_PROVIDERS

        return harness in READABLE_PROVIDERS
    except Exception:  # noqa: BLE001 - degrade open
        return True


def _capacity_state(value: object) -> tuple[str, str]:
    """(state, window-note) from a capacity entry: a bare state string, or the
    detailed mapping ``runtime_capacity`` produces."""
    if isinstance(value, Mapping):
        state = str(value.get("state", "") or "unknown").lower()
        return state, str(value.get("window", "") or "")
    return str(value or "unknown").lower(), ""


def row_capacity(
    row: InventoryRow, capacity: Optional[Mapping[str, object]]
) -> tuple[str, str]:
    """(state, window-note) THIS row reads from the runtime capacity snapshot.

    A row naming an account reads that account's own answer from the detail
    mapping; an account the snapshot does not name reads ``unknown``
    (permitted). A row naming no account reads the harness-wide MAX
    aggregate, the correct answer for it.
    """
    value = (capacity or {}).get(row.harness, "unknown")
    if not row.account:
        return _capacity_state(value)
    if isinstance(value, Mapping):
        accounts = value.get("accounts")
        if isinstance(accounts, Mapping):
            state = str(accounts.get(row.account) or "unknown").lower()
            return state, str(value.get("window", "") or "")
    return "unknown", ""


def resolve_grid(
    difficulty: Optional[str],
    priority: Optional[str],
    capacity: Optional[Mapping[str, object]],
    *,
    constrain_harness: Optional[str] = None,
    substrate: Optional[str] = None,
    permission_mode: Optional[str] = None,
    role: Optional[str] = None,
    protected_role: Optional[str] = None,
    inventory: Optional[Inventory] = None,
    settings: object = None,
    snapshot: Optional[dict] = None,
) -> tuple[Optional[dict[str, str]], list[str]]:
    """Join intrinsic difficulty and priority with a live capacity snapshot.

    The grid is a default route only. ``capacity`` is supplied by the runtime
    seam so this resolver never reads accounts or the network; an explicit
    flag or profile field occupies its axis and the grid fills the rest
    (``constrain_harness`` = the harness axis is taken; it still picks model
    and effort within it). Unknown capacity PERMITS a candidate and records
    ``capacity=unknown-permitted``; only a positive ``exhausted``/``blocked``
    marker removes one. Returns ``(candidate|None, chain)``; the chain's last
    element is the terminal reason the caller receipts on every path.
    """
    inv = inventory if inventory is not None else resolve_inventory(
        settings=settings, snapshot=snapshot
    )
    band = (difficulty or "").strip().lower()
    prio = (priority or "p2").strip().lower()
    # Round up under uncertainty: an absent or unmapped difficulty resolves to
    # the strong band, never the cheap one (the failure is asymmetric).
    band = band if band in _BAND_FLOOR else _STRONG_BAND
    chain = [f"grid difficulty({band}) priority({prio})"]
    if prio not in {"p0", "p1", "p2", "p3"}:
        chain.append("grid=invalid-input")
        return None, chain
    # Reads `declared`, not `rows`: the built-in fallback seeds rows, and the
    # grid stays config-first on purpose. A virgin install injects nothing and
    # says so, exactly as before the fallback existed.
    if not inv.declared or not inv.rows:
        chain.append("grid=no-inventory-declared")
        return None, chain

    # p0 gets the high-urgency band, p3 intentionally prefers the low-cost
    # band; p1/p2 preserve the filer's intrinsic difficulty. The planning role
    # floors at the strong end: a session that will blueprint first bills at
    # the planning tier, and a plan is what earns the cheap execution tier.
    candidate_band = "high" if prio == "p0" else "low" if prio == "p3" else band
    if (role or "").strip().lower() == "planning":
        candidate_band = _max_band(candidate_band, _PLANNING_BAND)
        chain.append(f"grid role(planning) floors band({_PLANNING_BAND})")
    if protected_role:
        from fno.agents.model_routing import PROTECTED_ROLE_FLOOR

        floor = PROTECTED_ROLE_FLOOR
        candidate_band = _max_band(candidate_band, floor)
        inv = dataclasses.replace(inv, objective="best-available")
        chain.append(f"grid protected-role({protected_role}) floor={floor}")

    rows = list(inv.rows.values())
    if constrain_harness:
        rows = [r for r in rows if r.harness == constrain_harness]
        chain.append(f"grid constrained to harness({constrain_harness})")
    before_filters = len(rows)
    rows = [
        r for r in rows
        if _candidate_supported(r.harness, substrate, permission_mode)
    ]
    if substrate or permission_mode:
        if not rows and before_filters:
            chain.append("grid=constrained-empty")
            return None, chain
        chain.append(
            f"grid filtered by substrate({substrate or '-'}) permission({permission_mode or '-'})"
        )

    # A declared row whose harness fno cannot drive REFUSES by name (AC3-ERR):
    # an uninstalled harness is a fact the receipt must carry, not an absence
    # silently skipped from the candidate list.
    installed: list[InventoryRow] = []
    for r in rows:
        if not r.harness or not r.model or _harness_installed(r.harness):
            installed.append(r)
        else:
            chain.append(f"grid refuses {r.name}: harness {r.harness!r} not installed")
    rows = installed

    # Tier wins: a row is a candidate when its band meets or exceeds the
    # requested floor. A row with NO band is a candidate at every band and
    # ranks after the banded rows that clear, in declared order: the operator
    # who declares no band declines to rank by strength, so fno does not rank
    # for them. No degrade below the floor here, unlike resolve_tier: the
    # grid's round-up ruling would be undone by quietly handing strong work to
    # a weak row, so an empty tier falls through to the operator's own
    # defaults instead.
    floor_rank = _BAND_RANK[candidate_band]
    clearing = [r for r in rows if r.rank >= floor_rank and r.harness and r.model]
    unbanded = [r for r in rows if r.band == "" and r.harness and r.model]
    if not clearing and not unbanded:
        chain.append("grid=no-band-candidate")
        return None, chain

    for row in _order_candidates(clearing, inv) + unbanded:
        state, window = row_capacity(row, capacity)
        if state in ("exhausted", "blocked"):
            chain.append(f"grid skip {row.harness}/{row.name} capacity={state}")
            continue
        if state not in ("ok", "low", "available"):
            state = "unknown-permitted"
        chain.append(
            f"grid candidate {row.harness}/{row.name} capacity={state}"
            + (f" window={window}" if window else "")
            + ("" if row.band else " band=unbanded")
        )
        out = {"harness": row.harness, "model": row.model}
        effort = row.effort
        if effort:
            try:
                from fno.agents.mux_spawn import effort_tokens

                effort_tokens(row.harness, effort)
            except Exception:  # noqa: BLE001 - no effort surface: inject nothing
                chain.append(f"grid effort omitted (no surface on {row.harness})")
                effort = ""
        if effort:
            out["effort"] = effort
            chain.append(f"grid effort({effort})")
        return out, chain
    # Reaching here means every candidate was skipped on a positive
    # exhausted/blocked marker (unknown permits and returns in-loop).
    chain.append("grid=no-available-candidate")
    return None, chain


_SLOT_LANE_FIELDS = (
    "provider", "model", "effort", "substrate", "permission_mode",
    "route", "account", "pane_group",
)
_LANE_PASSTHROUGH_FIELDS = ("substrate", "permission_mode", "pane_group")
_ON_EXHAUSTED = ("queue", "degrade", "refuse")
#: The verbs fno dispatches, and therefore the slots an operator fills.
SLOT_VERBS = ("think", "blueprint", "target", "review", "crown")


def _lane_field(lane: object, name: str) -> str:
    value = lane.get(name, "") if isinstance(lane, Mapping) else getattr(lane, name, "")
    return value.strip() if isinstance(value, str) else ""


def _slot_fold(
    lanes: Sequence[object],
    rung_base: str,
    settings: object,
    chain: list[str],
) -> tuple[Optional[list[tuple[str, str]]], Optional[Inventory], dict[str, dict[str, str]]]:
    """Fold lanes to ``(plan, row-inventory, inline-fields-by-rung)``.

    A string lane names a declared ``[[routing.models]]`` row; an inline
    table folds as its own row named by its config path. A config fault
    appends one ``slot=config`` terminal to ``chain`` and answers
    ``(None, None, {})``.
    """
    plan: list[tuple[str, str]] = []
    fold: list[dict[str, Any]] = []
    fields_by_rung: dict[str, dict[str, str]] = {}

    def _fault(rung: str, why: str) -> tuple[None, None, dict]:
        chain.append(f"slot=config {rung} {why}")
        return None, None, {}

    for index, raw in enumerate(lanes):
        rung = f"{rung_base}.lanes[{index}]"
        if isinstance(raw, str):
            if not raw.strip():
                return _fault(rung, "is an empty lane name")
            plan.append((rung, raw.strip()))
            continue
        if not isinstance(raw, Mapping) and not hasattr(raw, "provider"):
            return _fault(rung, "must be a table or a [[routing.models]] row name")
        if isinstance(raw, Mapping):
            unknown = sorted(set(raw) - set(_SLOT_LANE_FIELDS))
            if unknown:
                return _fault(rung, f"has unknown field {unknown[0]!r}")
            for key, value in raw.items():
                if not isinstance(value, str):
                    return _fault(rung, f".{key} must be a string; got {value!r}")
        fields = {key: _lane_field(raw, key) for key in _SLOT_LANE_FIELDS}
        if not any(fields.values()):
            return _fault(rung, "is empty")
        entry: dict[str, Any] = {"name": rung}
        for key in _SLOT_LANE_FIELDS:
            if fields[key] and key not in _LANE_PASSTHROUGH_FIELDS:
                entry["harness" if key == "provider" else key] = fields[key]
        fold.append(entry)
        fields_by_rung[rung] = fields
        plan.append((rung, rung))
    try:
        cfg_rows = list(getattr(getattr(settings, "routing", None), "models", None) or [])
    except Exception:  # noqa: BLE001
        cfg_rows = []
    return plan, inventory_from_rows(cfg_rows + fold, declared=True), fields_by_rung


def resolve_slot(
    verb: Optional[str],
    node: Optional[dict],
    capacity: Optional[Mapping[str, object]],
    *,
    inventory: Optional[Inventory] = None,
    settings: object = None,
    substrate: Optional[str] = None,
    permission_mode: Optional[str] = None,
    constrain_harness: Optional[str] = None,
    role: Optional[str] = None,
    protected_role: Optional[str] = None,
    model_occupied: bool = False,
    explicit_lane: bool = False,
) -> tuple[Optional[dict[str, Any]], list[str]]:
    """Which lane does this dispatch ride right now: the ONE slot resolver.

    ``agents.profiles.<verb>.lanes`` is the rank. Each lane is a
    ``[[routing.models]]`` row name or an inline table; the first lane whose
    posture, vendor cap and per-account capacity pass is the candidate
    (``harness``/``model``/``effort`` plus ``lane``/``lane_rung`` and the
    passthrough ``lane_fields``). ``on_exhausted`` names the all-skipped
    terminal; a command-line lane or ``FNO_SPAWN_GATE=0`` degrades whatever
    it says. No ``lanes``: fall through to :func:`resolve_grid` unchanged
    (a node-less spawn answers nothing). A config fault is a ``slot=config``
    terminal, never a raise. Returns ``(candidate, chain)`` like
    :func:`resolve_grid`.
    """
    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:  # noqa: BLE001 - a config read never breaks a spawn
            settings = None
    profile = _verb_profile(settings, verb)
    lanes = getattr(profile, "lanes", None) if profile is not None else None
    rung_base = f"agents.profiles.{verb}" if verb else "agents.profiles"
    if not lanes:
        if node is None:
            return None, []
        chain = [f"slot {rung_base} has no lanes; grid over inventory"]
        if model_occupied:
            chain.append("grid=model-axis-occupied")
            return None, chain
        candidate, grid_chain = resolve_grid(
            node.get("difficulty"),
            node.get("priority"),
            capacity,
            constrain_harness=constrain_harness,
            substrate=substrate,
            permission_mode=permission_mode,
            role=role,
            protected_role=protected_role,
            inventory=inventory,
        )
        return candidate, chain + grid_chain

    chain = [f"slot {rung_base} lanes walked in declared order"]
    raw_exhausted = str(getattr(profile, "on_exhausted", "") or "refuse")
    on_exhausted = raw_exhausted.strip().lower()
    if on_exhausted not in _ON_EXHAUSTED:
        chain.append(
            f"slot=config {rung_base}.on_exhausted {raw_exhausted!r} is not "
            f"one of {'|'.join(_ON_EXHAUSTED)}"
        )
        return None, chain

    plan, lane_inv, fields_by_rung = _slot_fold(lanes, rung_base, settings, chain)
    if plan is None or lane_inv is None:
        return None, chain

    import os

    gate_bypassed = os.environ.get("FNO_SPAWN_GATE") == "0"
    from fno.agents.spawn_gate import (
        ProviderCountUnavailable,
        provider_lanes_cap,
        provider_live_count,
    )
    from fno.config import provider_limits_table

    try:
        caps: dict = dict(provider_limits_table(getattr(settings, "agents", None)))
    except Exception:  # noqa: BLE001 - an unreadable cap table skips no lane
        caps = {}

    for index, (rung, row_name) in enumerate(plan):
        row = lane_inv.rows.get(row_name)
        if row is None:
            declared = ", ".join(sorted(lane_inv.rows)) or "(none)"
            chain.append(
                f"slot=config {rung} names no [[routing.models]] row "
                f"{row_name!r}; declared rows: {declared}; "
                "see fno config route inventory"
            )
            return None, chain
        if not _candidate_supported(row.harness, substrate, permission_mode):
            chain.append(
                f"slot skip {rung} {row_name} harness {row.harness!r} cannot "
                f"carry substrate({substrate or '-'}) "
                f"permission({permission_mode or '-'})"
            )
            continue
        vendor: Optional[str] = None
        if row.route:
            vendor = row.route.replace(",", "/").partition("/")[0].strip() or None
        cap = provider_lanes_cap(caps.get(vendor)) if vendor else None
        if vendor is not None and cap is not None:
            try:
                current = provider_live_count(vendor)
            except ProviderCountUnavailable as exc:
                if not gate_bypassed:
                    chain.append(
                        f"slot=provider-count-unavailable {rung} {vendor}: {exc}"
                    )
                    return None, chain
                chain.append(
                    f"slot note {rung} {row_name} provider count unavailable "
                    f"for {vendor}: {exc}; FNO_SPAWN_GATE=0, so the lane is "
                    "taken uncapped"
                )
            else:
                if current >= cap:
                    chain.append(
                        f"slot skip {rung} {row_name} provider {vendor} "
                        f"at {current} of {cap}"
                    )
                    continue
        state, window = row_capacity(row, capacity)
        if state in ("exhausted", "blocked"):
            chain.append(f"slot skip {rung} {row_name} capacity={state}")
            continue
        if state not in ("ok", "low", "available"):
            state = "unknown-permitted"
        chain.append(
            f"slot {rung} {row_name} capacity={state}"
            + (f" window={window}" if window else "")
        )
        inline = fields_by_rung.get(rung)
        lane_fields = dict(inline) if inline else {
            key: value
            for key, value in (
                ("provider", row.harness),
                ("model", row.model),
                ("effort", row.effort),
                ("route", row.route),
                ("account", row.account),
            )
            if value
        }
        pick: dict[str, Any] = {
            "harness": row.harness,
            "model": row.model,
            "lane": row_name,
            "lane_rung": rung,
            "lane_index": index,
            "lane_fields": lane_fields,
        }
        if row.effort:
            pick["effort"] = row.effort
        return pick, chain

    if explicit_lane or gate_bypassed:
        why = (
            "the command line already names the lane"
            if explicit_lane
            else "FNO_SPAWN_GATE=0"
        )
        chain.append(f"slot=exhausted degrade ({why})")
        return None, chain
    chain.append(f"slot=exhausted {on_exhausted}")
    return None, chain


def _max_band(a: str, b: str) -> str:
    return a if _BAND_RANK.get(a, -1) >= _BAND_RANK.get(b, -1) else b


def _verb_profile(settings: object, verb: Optional[str]) -> Optional[object]:
    """The verb's profile block, or None; never raises."""
    if not verb or settings is None:
        return None
    try:
        profiles = getattr(getattr(settings, "agents", None), "profiles", None) or {}
        return profiles.get(verb)
    except Exception:  # noqa: BLE001
        return None


def slot_states(
    verb: str,
    capacity: Optional[Mapping[str, object]],
    *,
    inventory: Optional[Inventory] = None,
    settings: object = None,
) -> dict[str, Any]:
    """The readout projection of one verb's slot: declared lanes in order with
    live :func:`row_capacity` states, the ``on_exhausted`` terminal, and -
    resolved by :func:`resolve_slot` ITSELF, never re-derived here - the lane
    a spawn would take right now. Display, never selection.
    """
    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:  # noqa: BLE001 - an unreadable config reads as absent
            settings = None
    if inventory is None:
        inventory = resolve_inventory(settings=settings)
    profile = _verb_profile(settings, verb)
    lanes = getattr(profile, "lanes", None) if profile is not None else None
    out: dict[str, Any] = {"verb": verb, "lanes": [], "on_exhausted": "", "would_take": ""}
    if not lanes:
        if inventory.declared and inventory.rows:
            out["would_take"] = f"no lanes; grid over {len(inventory.rows)} rows"
        else:
            out["would_take"] = "no lanes; no inventory; harness default"
        return out
    raw_exhausted = str(getattr(profile, "on_exhausted", "") or "refuse")
    on_exhausted = raw_exhausted.strip().lower()
    out["on_exhausted"] = (
        on_exhausted if on_exhausted in _ON_EXHAUSTED else f"{raw_exhausted} (invalid)"
    )
    chain: list[str] = []
    plan, lane_inv, _fields = _slot_fold(lanes, f"agents.profiles.{verb}", settings, chain)
    if plan is None or lane_inv is None:
        out["would_take"] = chain[-1]
        return out
    for rung, row_name in plan:
        row = lane_inv.rows.get(row_name)
        state = "no-such-row" if row is None else row_capacity(row, capacity)[0]
        out["lanes"].append({"rung": rung, "name": row_name, "state": state})
    candidate, slot_chain = resolve_slot(
        verb, None, capacity, inventory=inventory, settings=settings
    )
    if candidate is not None and candidate.get("lane_rung"):
        out["would_take"] = f"{candidate['lane_rung']} {candidate['lane']}"
    elif slot_chain:
        out["would_take"] = slot_chain[-1]
    return out


def harness_accounts(
    harness: str, *, settings: object = None, inventory: Optional[Inventory] = None
) -> list[str]:
    """Expand a harness to the ACCOUNT record ids reachable through it.

    Quota is a property of an ACCOUNT at a vendor; a harness is a client that
    can speak for several accounts. The direction is deliberate (M2): expand
    the harness to its accounts and aggregate, never fold records to harnesses.
    The account set is a UNION: every registered ``config.accounts.records``
    entry bound to the harness is reachable through it (one healthy account
    means the harness is usable), plus any inventory row ``account`` / ``route``
    vendor the records list does not already name.
    """
    inv = inventory if inventory is not None else resolve_inventory(settings=settings)
    accounts: list[str] = []
    for row in inv.rows.values():
        if row.harness != harness:
            continue
        accounts.extend(row.accounts())
    try:
        if settings is None:
            from fno.config import load_settings

            settings = load_settings()
        records = getattr(getattr(settings, "accounts", None), "records", None) or []
    except Exception:  # noqa: BLE001 - no config read is a dead spawn
        records = []
    for record in records:
        if not isinstance(record, Mapping):
            continue
        rid = record.get("id")
        bound = record.get("harness") or record.get("cli")
        if rid and bound == harness:
            accounts.append(str(rid))
    return list(dict.fromkeys(accounts))


def runtime_capacity(
    providers: tuple[str, ...] = ("claude", "codex", "gemini", "opencode"),
    *,
    settings: object = None,
    inventory: Optional[Inventory] = None,
) -> dict[str, object]:
    """Cached harness capacity: expand each harness to its accounts, read each
    account's headroom, aggregate MAX.

    A harness is ``ok`` if ANY account reachable through it is ok, ``exhausted``
    only if EVERY account is exhausted, ``unknown`` when no account answers.
    Every harness NAMED by a declared row is covered alongside ``providers``,
    so an agy or custom-harness row is never silently unprobed. The value is a
    detail mapping ``{state, window, accounts}``; bare state strings (the old
    shape) still resolve via :func:`_capacity_state`. Never probes, never
    touches the network, and reads the state file ONCE for all accounts.
    """
    try:
        from fno.adapters.providers.runtime_state import headrooms

        inv = inventory if inventory is not None else resolve_inventory(settings=settings)
        harnesses = list(dict.fromkeys(
            [*providers, *(r.harness for r in inv.rows.values() if r.harness)]
        ))
        out: dict[str, object] = {}
        for harness in harnesses:
            accounts = harness_accounts(harness, settings=settings, inventory=inv)
            detail: dict[str, str] = {}
            best: Optional[str] = None
            window = "absent"
            for account, verdict in headrooms(accounts).items():
                state = verdict.state.value
                detail[account] = state
                if best is None or _CAPACITY_RANK.get(state, 1) > _CAPACITY_RANK.get(best, 1):
                    best = state
                    window = verdict.source or "unknown"
            out[harness] = {
                "state": best or "unknown",
                "window": window,
                "accounts": detail,
            }
        return out
    except Exception:  # noqa: BLE001 - unknown capacity never breaks dispatch
        return {}


def _scoped_rows(
    inventory: Inventory, provider: Optional[str]
) -> list[InventoryRow]:
    """Inventory rows a tier may pick from, scoped to one harness when asked."""
    return [
        r for r in inventory.rows.values()
        if r.harness and r.model and (provider is None or r.harness == provider)
    ]


def resolve_tier(
    tier: Optional[str],
    *,
    snapshot: Optional[dict] = None,
    provider: Optional[str] = None,
    inventory: Optional[Inventory] = None,
    settings: object = None,
) -> tuple[Optional[str], list[str]]:
    """Resolve a tier to a concrete declared model. Returns ``(model, chain)``.

    ``provider`` scopes the candidate set to one harness (Locked Decision 1): a
    band left empty by the filter falls through the remaining bands within the
    same harness, then to None (provider default) - never a foreign-harness
    model. ``model`` is None when nothing resolves (the caller uses the
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
    if inventory is not None:
        # The caller handed us the inventory. An empty one is an answer, not a
        # gap: honor it rather than reaching past the caller for a fleet it did
        # not name.
        if not inventory.rows:
            chain.append("no declared inventory -> provider default")
            return None, chain
        inv = inventory
    else:
        # The built-in fallback seeds this, so a tier request still names a
        # model on an install that declares nothing - review level resolves one
        # for every level, and answering None would drop `/code-review` to the
        # provider default everywhere. Config overrides and extends the seed.
        inv = resolve_inventory(settings=settings, snapshot=snapshot)
        if not inv.rows:
            chain.append("no declared inventory -> provider default")
            return None, chain

    rows = _scoped_rows(inv, provider)
    floor_rank = _BAND_RANK[band]
    clearing = [r for r in rows if r.rank >= floor_rank]
    if clearing:
        row = _order_candidates(clearing, inv)[0]
        chain.append(f"inventory band(>={band}) -> {row.name}")
        return row.model, chain
    below = [r for r in rows if 0 <= r.rank < floor_rank]
    if below:
        # Degrade, never block: fall to the best available below the floor.
        best = max(below, key=lambda r: (r.rank, r.percentile or -1.0))
        chain.append(f"inventory band(>={band}) empty -> degrade -> {best.name}")
        return best.model, chain
    chain.append("inventory has no reachable model -> provider default")
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
    inventory: Optional[Inventory] = None,
) -> tuple[Optional[str], str, list[str]]:
    """Apply the full precedence chain. Returns ``(model, decision_source, chain)``.

    ``model`` is None only when everything falls through to the provider default.
    ``decision_source`` is the receipt vocabulary
    (``explicit`` / ``task-pin`` / ``task-difficulty(<band>)`` / ``plan-default`` /
    ``plan-difficulty(<band>)`` / ``provider-default(no-difficulty)``).
    ``provider`` scopes band
    resolution to one harness; pins (``explicit`` / ``task_model`` / ``plan_model``)
    bypass the filter - operator authority outranks routing (Locked Decision 4).
    """
    if explicit:
        return explicit, "explicit", ["explicit"]
    if task_model:
        return task_model, "task-pin", ["task-pin"]
    if task_difficulty:
        model, chain = resolve_tier(
            task_difficulty, snapshot=snapshot, provider=provider, inventory=inventory
        )
        return model, f"task-difficulty({task_difficulty.strip().lower()})", chain
    if plan_model:
        return plan_model, "plan-default", ["plan-default"]
    if plan_difficulty:
        model, chain = resolve_tier(
            plan_difficulty, snapshot=snapshot, provider=provider, inventory=inventory
        )
        return model, f"plan-difficulty({plan_difficulty.strip().lower()})", chain
    return None, "provider-default(no-difficulty)", ["provider-default(no-difficulty)"]


def node_model(
    node: dict,
    *,
    explicit: Optional[str] = None,
    snapshot: Optional[dict] = None,
    provider: Optional[str] = None,
    resolve_difficulty: bool = True,
    inventory: Optional[Inventory] = None,
) -> Optional[str]:
    """Concrete ``--model`` for a node/task at the spawn seam, or None for default.

    Reads the node's own ``model`` pin and ``difficulty`` band and applies
    the precedence with an optional dispatch-time ``explicit`` override.
    ``provider`` scopes band resolution to the spawn harness so a band never
    yields a cross-harness ``<provider> --model <foreign>`` pick. When ``provider`` is
    None it defaults to ``claude`` - the bg substrate's own spawn default (see
    ``advance._spawn_worker``: ``(provider or "").strip() or "claude"``, NOT the
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
            inventory=inventory,
        )
        return model
    except Exception:  # noqa: BLE001 - routing degrades, never blocks a spawn
        return explicit if explicit is not None else node.get("model")
