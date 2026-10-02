"""Dispatch-time model resolution: the config-first router's read side.

The declared inventory (``config.routing.models``) is the PRIMARY routing
surface: adding a model, provider or harness is a config edit. The snapshot
is OPTIONAL enrichment and can never make the grid inert; a virgin install
records ``grid=no-inventory-declared`` and injects nothing. Axis rule: an
explicit flag or profile field occupies the axis it names and nothing more
(``--model > task > plan > provider default``). Fields:
docs/architecture/role-based-model-routing.md.

The config gather and the inventory fold live in Rust now
(crates/fno-agents/src/route_gather.rs); this module is the transport: it
sends the verb the explicit inputs and nothing gathered, so the binary's
answers are the only routing decisions.
"""
from __future__ import annotations

import dataclasses
from typing import Any, Mapping, Optional

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


@dataclasses.dataclass(frozen=True)
class InventoryRow:
    """One resolved inventory row; ``band`` is "" when unbanded (a candidate
    at every band, ranked after the banded rows that clear)."""

    name: str
    harness: str
    model: str
    route: str = ""
    account: str = ""
    band: str = ""
    percentile: Optional[float] = None
    effort: str = ""
    cost_per_mtok_in: Optional[float] = None
    # The declared access path's verified native-view label; empty is
    # unverified. Qualification metadata: carried, never ranked.
    operator_view: str = ""

    @property
    def rank(self) -> int:
        return _BAND_RANK.get(self.band, -1)


@dataclasses.dataclass(frozen=True)
class Inventory:
    """The resolved inventory plus its config-owned objective. ``declared``
    says whether CONFIG named any row: a virgin install injects nothing."""

    rows: dict[str, InventoryRow] = dataclasses.field(default_factory=dict)
    objective: str = _OBJECTIVES[0]
    prefer_harness: str = ""
    declared: bool = False


def _inventory_from_answer(answer: Mapping[str, Any]) -> Inventory:
    """The verb's ``mode: "inventory"`` answer as an Inventory: rows in
    declared order, the objective already validated on the Rust side."""
    rows: dict[str, InventoryRow] = {}
    for row in answer.get("rows") or []:
        if not isinstance(row, Mapping):
            continue
        name = str(row.get("name", "") or "").strip()
        if not name:
            continue
        pct = row.get("percentile")
        rows[name] = InventoryRow(
            name=name,
            harness=str(row.get("harness", "") or ""),
            model=str(row.get("model", "") or ""),
            route=str(row.get("route", "") or ""),
            account=str(row.get("account", "") or ""),
            band=str(row.get("band", "") or ""),
            percentile=float(pct) if isinstance(pct, (int, float)) else None,
            effort=str(row.get("effort", "") or ""),
            cost_per_mtok_in=(
                float(row["cost_per_mtok_in"])
                if isinstance(row.get("cost_per_mtok_in"), (int, float))
                else None
            ),
            operator_view=str(row.get("operator_view", "") or ""),
        )
    objective = str(answer.get("objective", "") or "")
    if objective not in _OBJECTIVES:
        objective = _OBJECTIVES[0]
    return Inventory(
        rows=rows,
        objective=objective,
        prefer_harness=str(answer.get("prefer_harness", "") or ""),
        declared=bool(answer.get("declared")),
    )


def resolve_inventory() -> Inventory:
    """Read the declared inventory from config. Never raises: an unloadable
    config or a missing verb is an EMPTY inventory, not a dead spawn."""
    from fno.route_slot_client import route_slot_call

    try:
        return _inventory_from_answer(route_slot_call({"mode": "inventory"}))
    except Exception:  # noqa: BLE001 - a routing read never breaks a spawn
        return Inventory()


#: The verbs fno dispatches, and therefore the slots an operator fills.
#: PR creation runs inline in the invoking session; it dispatches no slot.
SLOT_VERBS = ("think", "blueprint", "target", "review", "crown")


def slot_verbs() -> list[str]:
    """Every verb the readout should show: the dispatched verbs plus any
    profile carrying lane configuration (the inventory answer's verbs)."""
    from fno.route_slot_client import route_slot_call

    try:
        answer = route_slot_call({"mode": "inventory"})
        verbs = [str(v) for v in answer.get("verbs") or [] if str(v)]
    except Exception:  # noqa: BLE001 - a read failure shows the known verbs
        verbs = []
    return verbs or list(SLOT_VERBS)


def _node_payload(node: Optional[Mapping]) -> Optional[dict]:
    if not node:
        return None
    return {
        "difficulty": node.get("difficulty"),
        "priority": node.get("priority"),
        "plan_path": str(node.get("plan_path") or ""),
    }


def resolve_slot(
    verb: Optional[str],
    node: Optional[dict],
    capacity: Optional[Mapping[str, object]],
    *,
    substrate: Optional[str] = None,
    permission_mode: Optional[str] = None,
    constrain_harness: Optional[str] = None,
    role: Optional[str] = None,
    protected_role: Optional[str] = None,
    model_occupied: bool = False,
    explicit_lane: bool = False,
    work_verb: Optional[str] = None,
    explicit_model_value: Optional[str] = None,
    explicit_route_value: Optional[str] = None,
    explicit_vendor_value: Optional[str] = None,
    meta: Optional[dict[str, Any]] = None,
    capacity_refresh: bool = False,
) -> tuple[Optional[dict[str, Any]], list[str], str]:
    """Which lane does this dispatch ride right now: the ONE slot resolver.
    Selection is Rust (``fno-agents route-slot``); chain strings come back
    verbatim, and a missing or failing binary is a named refusal. The
    config-derived payload keys (declared rows, the slot table, the policy)
    are the verb's to gather now: this transport sends the explicit inputs
    and nothing else. ``work_verb`` is the original dispatch command,
    audit-only since the derived verb IS the command. The third element is
    the walk's own verdict word: armed, unarmed, or a hold. Callers that need
    the structured refusal pass ``meta``; it is filled with the verb's
    ``refusal_terminal`` object when the answer carries one."""
    import os

    rung_base = f"agents.profiles.{verb}" if verb else "agents.profiles"
    gate_bypassed = os.environ.get("FNO_SPAWN_GATE") == "0"
    from fno.route_slot_client import RouteSlotUnavailable, route_slot_call

    payload: dict[str, Any] = {
        "rung_base": rung_base,
        "node": _node_payload(node),
        "substrate": substrate,
        "permission_mode": permission_mode,
        "constrain_harness": constrain_harness,
        "explicit_lane": explicit_lane,
        "gate_bypassed": gate_bypassed,
        "role": role,
        "protected_role": protected_role,
        "model_occupied": model_occupied,
        "work_verb": work_verb or verb,
        "explicit_model_value": explicit_model_value,
        "explicit_route_value": explicit_route_value,
        "explicit_vendor_value": explicit_vendor_value,
    }
    if capacity is not None:
        payload["capacity"] = dict(capacity)
    if capacity_refresh:
        payload["capacity_refresh"] = True
    try:
        out = route_slot_call(payload, timeout=90.0 if capacity_refresh else 30.0)
    except RouteSlotUnavailable as exc:
        # The transport fault never reaches the verb, so the Python side owns
        # this one refusal composition: same shape the verb answers with.
        text = f"route-slot-unavailable ({exc});"
        if _routing_enforced():
            text += " (strict routing: config routing.enforce_inventory)"
        if meta is not None:
            meta["refusal"] = {"class": "unavailable", "text": text}
        return None, [f"slot=route-slot-unavailable ({exc})"], "unarmed"
    chain = [str(line) for line in (out.get("chain") or [])]
    if meta is not None:
        if isinstance(out.get("refusal_terminal"), dict):
            meta["refusal"] = out["refusal_terminal"]
        if isinstance(out.get("exhausted_payload"), dict):
            meta["exhausted"] = out["exhausted_payload"]
        meta["fingerprint"] = str(out.get("fingerprint") or "")
        if isinstance(out.get("capacity"), dict):
            meta["capacity"] = out["capacity"]
    return out.get("candidate"), chain, str(out.get("verdict") or "unarmed")


def _routing_enforced() -> bool:
    """The strict-routing flag through the verb's policy mode; an unreadable
    flag reads as off."""
    from fno.route_slot_client import route_slot_call

    try:
        answer = route_slot_call({"mode": "policy"})
        return bool(answer.get("enforce_inventory"))
    except Exception:  # noqa: BLE001 - an unreadable flag reads as off
        return False


def _answer(out: dict[str, Any], key: str) -> tuple[Any, list[str]]:
    """The verb's named field plus its chain, lines coerced verbatim."""
    return out.get(key), [str(line) for line in (out.get("chain") or [])]


def slot_states(verb: str, capacity: Optional[Mapping[str, object]] = None) -> dict[str, Any]:
    """Readout of one verb's slot: lanes, policy lines, and would_take - the
    verb's own answer. Display, never selection."""
    rung_base = f"agents.profiles.{verb}"
    payload: dict[str, Any] = {
        "rung_base": rung_base,
        "node": None,
        "substrate": None,
        "permission_mode": None,
        "constrain_harness": None,
        "explicit_lane": False,
        "gate_bypassed": False,
        "model_occupied": False,
        "work_verb": verb,
        "mode": "states",
    }
    if capacity is not None:
        payload["capacity"] = dict(capacity)
    states: dict[str, Any] = {}
    try:
        from fno.route_slot_client import route_slot_call

        states = route_slot_call(payload)
    except Exception as exc:  # noqa: BLE001 - a missing verb degrades the readout
        states = {"would_take": f"slot=route-slot-unavailable ({exc})"}
    out: dict[str, Any] = {
        "verb": verb, "lanes": [], "on_exhausted": "", "would_take": "",
        "routing": "unarmed",
    }
    for key in (
        "on_exhausted", "on_low", "on_unknown", "would_take", "routing",
        "work_kind", "operator_access", "policy_source", "skipped",
        "fingerprint",
    ):
        if key in states:
            out[key] = states[key]
    # The difficulty note is the verb's vocabulary: take it back verbatim.
    for line in states.get("chain") or []:
        note_prefix = f"slot note {rung_base} "
        if line.startswith(note_prefix):
            out["note"] = line[len(note_prefix):]
    out["lanes"] = [
        {k: str(e.get(k, "" if k != "state" else "unknown"))
         for k in ("rung", "name", "state")}
        | {k: str(e[k]) for k in ("identity", "source") if e.get(k)}
        for e in states.get("lane_states") or []
    ]
    return out


def resolve_tier(
    tier: Optional[str],
    *,
    provider: Optional[str] = None,
) -> tuple[Optional[str], list[str]]:
    """Resolve a tier to a concrete declared model, scoped to one harness when
    asked. The band math lives on ``fno-agents route-slot``; never raises."""
    from fno.route_slot_client import RouteSlotUnavailable, route_slot_call

    try:
        return _answer(route_slot_call(
            {"mode": "tier", "tier": tier, "provider": provider}), "model")
    except RouteSlotUnavailable:
        return None, ["tier=route-slot-unavailable"]


def resolve_dispatch_model(
    *,
    explicit: Optional[str] = None,
    task_model: Optional[str] = None,
    task_difficulty: Optional[str] = None,
    plan_model: Optional[str] = None,
    plan_difficulty: Optional[str] = None,
    provider: Optional[str] = None,
) -> tuple[Optional[str], str, list[str]]:
    """Apply the full precedence chain; ``(model, decision_source, chain)``.
    The chain runs in the verb's dispatch_model mode; pins bypass it without
    a verb round trip - superuser authority outranks routing, and the old
    seam answered a pin in-process."""
    if explicit:
        return explicit, "explicit", ["explicit"]
    if task_model:
        return task_model, "task-pin", ["task-pin"]
    from fno.route_slot_client import RouteSlotUnavailable, route_slot_call

    try:
        out = route_slot_call({
            "mode": "dispatch_model",
            "explicit": explicit,
            "task_model": task_model,
            "task_difficulty": task_difficulty,
            "plan_model": plan_model,
            "plan_difficulty": plan_difficulty,
            "provider": provider,
        })
        return out.get("model"), str(out.get("source") or ""), [
            str(line) for line in (out.get("chain") or [])
        ]
    except RouteSlotUnavailable:
        return None, "provider-default(no-difficulty)", [
            "provider-default(no-difficulty)", "dispatch-model route-slot-unavailable",
        ]


def node_model(
    node: dict,
    *,
    explicit: Optional[str] = None,
    provider: Optional[str] = None,
    resolve_difficulty: bool = True,
) -> Optional[str]:
    """Concrete ``--model`` for a node at the spawn seam, or None for default.
    Strictly non-fatal: an error degrades to the explicit override or the
    node's raw pin."""
    try:
        model, _source, _chain = resolve_dispatch_model(
            explicit=explicit,
            task_model=node.get("model"),
            task_difficulty=node.get("difficulty") if resolve_difficulty else None,
            provider=provider or "claude",
        )
        return model
    except Exception:  # noqa: BLE001 - routing degrades, never blocks a spawn
        return explicit if explicit is not None else node.get("model")
