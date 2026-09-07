"""Dispatch-time model resolution: the config-first router's read side.

The declared inventory (``config.routing.models``) is the PRIMARY routing
surface: nothing built-in is authoritative, so adding a model, a provider or a
harness is a config edit. The OpenRouter snapshot is OPTIONAL enrichment and
can never make the grid inert; a virgin install records
``grid=no-inventory-declared`` and injects nothing. Per AXIS (Locked
Decision 1): an explicit flag or a profile field occupies the axis it names
and nothing more - ``dispatch --model > task model > task difficulty >
plan model > plan difficulty > provider default``. Field semantics:
docs/architecture/role-based-model-routing.md.
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
_BAND_RANK = {"low": 0, "medium": 1, "high": 2, "max": 3}

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
_OBJECTIVES = ("cheapest-that-clears", "best-available", "prefer-harness")

# Aggregation order for a harness's accounts: MAX over headroom. ok > low >
# unknown > exhausted. Unknown outranks exhausted because exhaustion is only
# true when EVERY account says so (M2/t2.1): one silent account never walls a
# harness another account can still serve. The MAX aggregate is the
# HARNESS-WIDE answer and is correct for a row that names no account; a row
# that names an account gets that account's own answer from the detail map.
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

        ``route`` deliberately contributes nothing: it names a VENDOR lane,
        and folding it in would add a pseudo-account whose permanent UNKNOWN
        dilutes a real account's live lock in the MAX aggregate.
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














_ON_EXHAUSTED = ("queue", "degrade", "refuse")
#: The verbs fno dispatches, and therefore the slots an operator fills.
SLOT_VERBS = ("think", "blueprint", "target", "review", "crown")


def slot_verbs(settings: object = None, inventory: Optional[Inventory] = None) -> list[str]:
    """Every verb whose slot the readout should show: the dispatched verbs
    plus any profile that carries lane configuration. A verb armed only
    through an unlisted profile key must still render, or the doctor would
    call a half-armed router unarmed.
    """
    verbs = list(SLOT_VERBS)
    if settings is None:
        settings, _profile, _lanes = _slot_entry(None, None)
    try:
        profiles = getattr(getattr(settings, "agents", None), "profiles", None) or {}
        for key in profiles:
            if key not in verbs and key:
                verbs.append(str(key))
    except Exception:  # noqa: BLE001 - a read failure shows the known verbs
        pass
    return verbs


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
    explicit_model: bool = False,
    explicit_lane: bool = False,
) -> tuple[Optional[dict[str, Any]], list[str]]:
    """Which lane does this dispatch ride right now: the ONE slot resolver.

    The selection core is Rust (``fno-agents route-slot``, law d-450caaeb):
    ``agents.profiles.<verb>.lanes`` is the rank, walked in declared order,
    and the first lane whose posture, vendor cap, identity and per-account
    capacity pass is the candidate. ``on_exhausted`` names the all-skipped
    terminal; a command-line lane or ``FNO_SPAWN_GATE=0`` degrades whatever
    it says. No ``lanes`` (and no ``by_difficulty`` overlay): fall through to
    the verb's grid leg (a node-less spawn answers nothing). The
    chain strings are the receipt vocabulary and come back verbatim from the
    verb; a missing or failing binary is a named refusal, never a silent
    lane-less spawn.
    """
    import os

    settings, profile, lanes = _slot_entry(settings, verb)
    rung_base = f"agents.profiles.{verb}" if verb else "agents.profiles"
    by_diff = getattr(profile, "by_difficulty", None)
    has_overlay = isinstance(by_diff, Mapping) and bool(by_diff)
    if not lanes and not has_overlay and node is None:
        return None, []

    gate_bypassed = os.environ.get("FNO_SPAWN_GATE") == "0"
    from fno.route_slot_client import RouteSlotUnavailable, resolve_slot_via_binary

    try:
        return resolve_slot_via_binary(
            rung_base=rung_base,
            profile=profile,
            lanes=lanes,
            node=node,
            capacity=capacity,
            inventory=inventory,
            settings=settings,
            substrate=substrate,
            permission_mode=permission_mode,
            constrain_harness=constrain_harness,
            explicit_lane=explicit_lane,
            explicit_model=explicit_model,
            gate_bypassed=gate_bypassed,
            role=role,
            protected_role=protected_role,
            model_occupied=model_occupied,
        )
    except RouteSlotUnavailable as exc:
        return None, [f"slot=route-slot-unavailable ({exc})"]




def _verb_profile(settings: object, verb: Optional[str]) -> Optional[object]:
    """The verb's profile block, or None; never raises."""
    if not verb or settings is None:
        return None
    try:
        profiles = getattr(getattr(settings, "agents", None), "profiles", None) or {}
        return profiles.get(verb)
    except Exception:  # noqa: BLE001
        return None


def _slot_entry(
    settings: object, verb: Optional[str]
) -> tuple[object, Optional[object], Any]:
    """Settings (a config read never raises), the verb's profile, its lanes."""
    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:  # noqa: BLE001 - an unreadable config reads as absent
            settings = None
    profile = _verb_profile(settings, verb)
    lanes = getattr(profile, "lanes", None) if profile is not None else None
    return settings, profile, lanes


def _states_row_name(state_entry: Mapping, lanes: Any, lane_states: list) -> str:
    """The row a state entry names: a declared row keeps its name; an inline
    lane table folds as its own row named by the verb's rung."""
    rung = str(state_entry.get("rung", ""))
    try:
        index = lane_states.index(state_entry)
    except ValueError:
        return rung
    raw = lanes[index] if isinstance(lanes, (list, tuple)) and index < len(lanes) else None
    return raw.strip() if isinstance(raw, str) else rung


def slot_states(
    verb: str,
    capacity: Optional[Mapping[str, object]],
    *,
    inventory: Optional[Inventory] = None,
    settings: object = None,
) -> dict[str, Any]:
    """Readout of one verb's slot: lanes in order with live capacity states,
    identity and observation source per lane, the ``on_exhausted`` terminal,
    and the lane a spawn would take right now (resolved by
    :func:`resolve_slot` itself). Display, never selection; callers label it
    a preview.
    """
    settings, _profile, lanes = _slot_entry(settings, verb)
    if inventory is None:
        inventory = resolve_inventory(settings=settings)
    out: dict[str, Any] = {
        "verb": verb, "lanes": [], "on_exhausted": "", "would_take": "",
        "routing": "unarmed",
    }
    by_diff = getattr(_profile, "by_difficulty", None) or {}
    if not lanes and isinstance(by_diff, Mapping) and by_diff:
        # An overlay-only slot is armed: display the high overlay's lanes
        # (the default effective difficulty); the resolver derives per dispatch.
        overlay = by_diff.get("high")
        if isinstance(overlay, Mapping):
            lanes = overlay.get("lanes")
    if not lanes:
        if inventory.declared and inventory.rows:
            out["would_take"] = f"no lanes; grid over {len(inventory.rows)} rows"
        else:
            out["would_take"] = "no lanes; no inventory; harness default"
        return out
    raw_exhausted = str(getattr(_profile, "on_exhausted", "") or "refuse")
    on_exhausted = raw_exhausted.strip().lower()
    out["on_exhausted"] = (
        on_exhausted if on_exhausted in _ON_EXHAUSTED else f"{raw_exhausted} (invalid)"
    )
    out["on_low"] = str(getattr(_profile, "on_low", "") or "prefer_healthy")
    out["on_unknown"] = str(getattr(_profile, "on_unknown", "") or "allow")
    rung_base = f"agents.profiles.{verb}"
    try:
        from fno.route_slot_client import route_states_via_binary

        lane_states, states_chain = route_states_via_binary(
            rung_base=rung_base,
            profile=_profile,
            lanes=lanes,
            node=None,
            capacity=capacity,
            settings=settings,
        )
    except Exception:  # noqa: BLE001 - a missing verb degrades the readout
        lane_states, states_chain = [], []
    # Rungs and the difficulty note are the verb's vocabulary: take them back
    # from its output instead of rebuilding them here.
    for line in states_chain:
        note_prefix = f"slot note {rung_base} "
        if line.startswith(note_prefix):
            out["note"] = line[len(note_prefix):]
    # Inline lane tables fold as their own rows named by rung, so the readout
    # can show identity/source per lane; declared row names pass through.
    lane_inv = inventory_from_rows(
        list(inventory.rows.values())
        + [
            {
                "name": rung,
                "harness": str(raw.get("provider", "") or ""),
                "model": str(raw.get("model", "") or ""),
                "route": str(raw.get("route", "") or ""),
                "account": str(raw.get("account", "") or ""),
                "effort": str(raw.get("effort", "") or ""),
            }
            for rung, raw in (
                (str(lane_states[i].get("rung", "")), raw)
                for i, raw in enumerate(lanes)
                if isinstance(raw, Mapping) and i < len(lane_states)
            )
        ],
        declared=True,
    )
    for state_entry in lane_states:
        rung = str(state_entry.get("rung", ""))
        row_name = _states_row_name(state_entry, lanes, lane_states)
        row = lane_inv.rows.get(row_name)
        state = str(state_entry.get("state", "unknown"))
        entry: dict[str, Any] = {"rung": rung, "name": row_name, "state": state}
        out["lanes"].append(entry)
    candidate, slot_chain = resolve_slot(
        verb, None, capacity, inventory=inventory, settings=settings
    )
    if candidate is not None and candidate.get("lane_rung"):
        out["would_take"] = f"{candidate['lane_rung']} {candidate['lane']}"
        out["routing"] = "armed"
    else:
        out["routing"] = "unarmed"
        if slot_chain:
            out["would_take"] = slot_chain[-1]
    return out


def harness_accounts(
    harness: str, *, settings: object = None, inventory: Optional[Inventory] = None
) -> list[str]:
    """Expand a harness to the ACCOUNT record ids reachable through it.

    Quota is a property of an ACCOUNT; a harness is a client that can speak
    for several. The set is a UNION: registered records bound to the harness
    plus inventory-row accounts.
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


def _identity_evidence(harness: str, accounts: list[str]) -> dict[str, str]:
    """proven|mismatch per account, from the attribution owner alone.

    ``proven``: the record id the owner says is the CLI's active slot occupant
    on an untainted slot. Any other named account on a PROVEN slot is a
    ``mismatch`` - it pins a name the slot disproves. No owner answer, a
    tainted slot, or a store read failure leaves every account unnamed, which
    consumers read as ``account_identity_unknown``. This never reads
    credentials itself; x-d6be owns that and this consumes its verdicts.
    """
    try:
        from fno.adapters.providers.managed import (
            active_slot_id,
            slot_tainted,
            store_root,
        )

        active = active_slot_id(harness)
        if not active or slot_tainted(harness, store_root()):
            return {}
        return {a: ("proven" if a == active else "mismatch") for a in accounts}
    except Exception:  # noqa: BLE001 - an unreadable owner reads as unknown
        return {}


def runtime_capacity(
    providers: tuple[str, ...] = ("claude", "codex", "gemini", "opencode"),
    *,
    settings: object = None,
    inventory: Optional[Inventory] = None,
) -> dict[str, object]:
    """Cached harness capacity: expand each harness to its accounts, read each
    account's headroom, aggregate MAX (ok if ANY account is ok, exhausted only
    if EVERY account is). Every harness NAMED by a declared row is probed
    alongside ``providers``. The value is a detail mapping
    ``{state, window, accounts, evidence, resets}``; bare state strings still
    resolve as bare state strings too. When the attribution owner proves an
    active slot account, ITS state is the aggregate - MAX over the sibling
    records can never make canonical claude look healthy - and when the slot
    only yields mismatches the aggregate reads unknown. Never probes, never
    touches the network.
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
            resets: dict[str, object] = {}
            best: Optional[str] = None
            window = "absent"
            for account, verdict in headrooms(accounts).items():
                state = verdict.state.value
                detail[account] = state
                resets[account] = verdict.resets_at
                if best is None or _CAPACITY_RANK.get(state, 1) > _CAPACITY_RANK.get(best, 1):
                    best = state
                    window = verdict.source or "unknown"
            evidence = _identity_evidence(harness, accounts)
            proven = [a for a, v in evidence.items() if v == "proven"]
            if proven and detail.get(proven[0]):
                best = detail[proven[0]]
                window = f"identity:{proven[0]}"
            elif evidence and not proven and any(v == "mismatch" for v in evidence.values()):
                best = "unknown"
                window = "identity-unproven"
            out[harness] = {
                "state": best or "unknown",
                "window": window,
                "accounts": detail,
                "evidence": evidence,
                "resets": resets,
            }
        return out
    except Exception:  # noqa: BLE001 - unknown capacity never breaks dispatch
        return {}




def resolve_tier(
    tier: Optional[str],
    *,
    snapshot: Optional[dict] = None,
    provider: Optional[str] = None,
    inventory: Optional[Inventory] = None,
    settings: object = None,
) -> tuple[Optional[str], list[str]]:
    """Resolve a tier to a concrete declared model. Returns ``(model, chain)``.

    ``provider`` scopes the candidate set to one harness: a band the filter
    empties falls through the remaining bands within the same harness, then to
    None (provider default) - never a foreign-harness model. The band math and
    ordering live on ``fno-agents route-slot`` (mode ``tier``); this wrapper
    owns the inventory read and never raises.
    """
    from fno.route_slot_client import RouteSlotUnavailable, route_tier_via_binary

    inv = inventory if inventory is not None else resolve_inventory(
        settings=settings, snapshot=snapshot
    )
    try:
        return route_tier_via_binary(tier, provider, inv)
    except RouteSlotUnavailable:
        return None, ["tier=route-slot-unavailable"]


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

    ``model`` is None only when everything falls through to the provider
    default. Pins (``explicit`` / ``task_model`` / ``plan_model``) bypass the
    band filter - operator authority outranks routing (Locked Decision 4).
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

    Reads the node's ``model`` pin and ``difficulty`` band under the full
    precedence, with ``provider`` scoping bands to the spawn harness: None
    means ``claude`` - the bg substrate's own spawn default, NOT the ambient
    harness. Strictly non-fatal: any resolution error degrades to the explicit
    override or the node's raw ``model`` pin (Locked Decision 10).
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
