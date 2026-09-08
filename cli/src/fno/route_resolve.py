"""Dispatch-time model resolution: the config-first router's read side.

The declared inventory (``config.routing.models``) is the PRIMARY routing
surface: adding a model, provider or harness is a config edit. The snapshot
is OPTIONAL enrichment and can never make the grid inert; a virgin install
records ``grid=no-inventory-declared`` and injects nothing. Axis rule: an
explicit flag or profile field occupies the axis it names and nothing more
(``--model > task > plan > provider default``). Fields:
docs/architecture/role-based-model-routing.md.
"""
from __future__ import annotations

import dataclasses
import hashlib
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
    context: Optional[int] = None

    @property
    def rank(self) -> int:
        return _BAND_RANK.get(self.band, -1)

    def accounts(self) -> list[str]:
        """The account record id whose quota this row spends, if named.
        ``route`` names a VENDOR lane, not an account: folding it in would
        add a pseudo-account whose permanent UNKNOWN dilutes a real
        account's live lock in the MAX aggregate.
        """
        return [self.account] if self.account else []


@dataclasses.dataclass(frozen=True)
class Inventory:
    """The resolved inventory plus its config-owned objective. ``declared``
    says whether CONFIG named any row: a virgin install injects nothing."""

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
    """Fold declared rows into an :class:`Inventory` (later rows of one name
    override per field). Band: the row's own, else a percentile against
    ``_BAND_FLOOR``, else unbanded."""
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
    One row per name, the strongest band winning; config overrides per field
    and extends by name."""
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
    """Read the declared inventory from config. Never raises: an unloadable
    config is an EMPTY inventory, not a dead spawn."""
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














#: The verbs fno dispatches, and therefore the slots an operator fills.
SLOT_VERBS = ("think", "blueprint", "target", "review", "crown", "pr-create")


def slot_verbs(settings: object = None, inventory: Optional[Inventory] = None) -> list[str]:
    """Every verb the readout should show: the dispatched verbs plus any
    profile carrying lane configuration."""
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
    work_verb: Optional[str] = None,
    explicit_model_value: Optional[str] = None,
    explicit_route_value: Optional[str] = None,
    explicit_vendor_value: Optional[str] = None,
) -> tuple[Optional[dict[str, Any]], list[str]]:
    """Which lane does this dispatch ride right now: the ONE slot resolver.
    Selection is Rust (``fno-agents route-slot``); chain strings come back
    verbatim, and a missing or failing binary is a named refusal. ``work_verb``
    is the ORIGINAL dispatch command (a planless target plans: the command
    stays target while the slot is blueprint); it defaults to ``verb``."""
    import os

    settings, profile, lanes = _slot_entry(settings, verb)
    rung_base = f"agents.profiles.{verb}" if verb else "agents.profiles"
    by_diff = getattr(profile, "by_difficulty", None)
    has_overlay = isinstance(by_diff, Mapping) and bool(by_diff)
    if not lanes and not has_overlay and node is None and not _routing_enforced(settings):
        return None, []

    gate_bypassed = os.environ.get("FNO_SPAWN_GATE") == "0"
    from fno.route_slot_client import RouteSlotUnavailable, route_slot_call

    try:
        return _answer(route_slot_call(_slot_payload(
            rung_base=rung_base, profile=profile, lanes=lanes, node=node,
            capacity=capacity, inventory=inventory, settings=settings,
            substrate=substrate, permission_mode=permission_mode,
            constrain_harness=constrain_harness, explicit_lane=explicit_lane,
            explicit_model=explicit_model, gate_bypassed=gate_bypassed,
            role=role, protected_role=protected_role,
            model_occupied=model_occupied,
            work_verb=work_verb or verb,
            explicit_model_value=explicit_model_value,
            explicit_route_value=explicit_route_value,
            explicit_vendor_value=explicit_vendor_value,
        )), "candidate")
    except RouteSlotUnavailable as exc:
        return None, [f"slot=route-slot-unavailable ({exc})"]


def _routing_enforced(settings: object) -> bool:
    try:
        return bool(getattr(getattr(settings, "routing", None), "enforce_inventory", False))
    except Exception:  # noqa: BLE001 - an unreadable flag reads as off
        return False


def routing_fingerprint(settings: object = None) -> str:
    """A short fingerprint of the routing-relevant NONSECRET config inputs.

    The receipt answer to "was the config that decided this the config that
    launched": declared rows, policy fields and the slot table, with no
    credential values. A changed fingerprint says the next launch re-selects;
    it is never an ownership token.
    """
    try:
        payload = {
            "rows": sorted(_declared_rows(settings).items()),
            "policy": _routing_policy_payload(settings),
            "slots": _slot_profiles_table(settings),
        }
        text = repr(payload)
        return hashlib.sha256(text.encode()).hexdigest()[:12]
    except Exception:  # noqa: BLE001 - an unreadable config carries no fingerprint
        return ""





def _answer(out: dict[str, Any], key: str) -> tuple[Any, list[str]]:
    """The verb's named field plus its chain, lines coerced verbatim."""
    return out.get(key), [str(line) for line in (out.get("chain") or [])]


def slot_verdict(candidate: Any, chain: list[str]) -> str:
    """The readout verdict, read from the same terminal the seam refuses on.

    One classifier for every consumer: explain renders it, the spawn seam
    refuses on it, and the inventory preview echoes it. A policy refusal is
    held, capacity is held, and neither is "exhausted dispatch".
    """
    if candidate:
        return "armed"
    terminal = chain[-1] if chain else ""
    if "slot=strict-refusal" in terminal or terminal.startswith("slot=config "):
        return "policy-held"
    if terminal.startswith("slot=exhausted"):
        return "capacity-held"
    return "unarmed"


def _profile_fields(profile: Optional[object]) -> dict[str, Any]:
    by_diff = getattr(profile, "by_difficulty", None)
    return {
        **{k: str(getattr(profile, k, "") or "")
           for k in ("on_exhausted", "on_low", "on_unknown")},
        "by_difficulty": by_diff if isinstance(by_diff, Mapping) else {},
    }


_DECLARED_FIELDS = ("harness", "model", "route", "account", "band", "effort", "operator_view")


def _declared_rows(settings: object) -> dict[str, Any]:
    """The CONFIG-declared rows exactly (never the built-in fallback); rows
    arrive as pydantic models, and a repeated name folds per field."""
    try:
        models = getattr(getattr(settings, "routing", None), "models", None) or []
        if not models:
            return {}
        # one fold, one contract: the loader returns typed rows (BaseModel),
        # and a repeated name overrides per field, never wholesale
        folded = inventory_from_rows(models).rows
    except Exception:  # noqa: BLE001 - an unreadable config reads as empty
        return {}
    return {
        name: {"name": name,
               **{f: str(getattr(r, f, "") or "").strip() for f in _DECLARED_FIELDS}}
        for name, r in folded.items()
    }


def _lanes_payload(lanes: Any) -> list[Any]:
    """Lane entries as JSON; profile lane objects serialize by the verb's own fields."""
    fields = ("provider", "model", "effort", "substrate", "permission_mode",
              "route", "account", "pane_group")
    out: list[Any] = []
    for lane in lanes or []:
        if isinstance(lane, Mapping):
            out.append(dict(lane))
        elif hasattr(lane, "provider") or hasattr(lane, "model"):
            payload: dict[str, Any] = {k: str(getattr(lane, k, "") or "") for k in fields}
            # args is the lane's native-bundle vector (x-8975): opaque, passed
            # through verbatim, never one of the ranked fields.
            args = getattr(lane, "args", None)
            if args:
                payload["args"] = [str(a) for a in args]
            out.append(payload)
        else:
            out.append(lane)
    return out


def _inventory_payload(inventory: Optional[Any]) -> dict[str, Any]:
    """The resolved inventory as JSON: rows in declared order plus objective."""
    if inventory is None:
        return {}
    try:
        return {
            "declared": bool(getattr(inventory, "declared", False)),
            "objective": str(getattr(inventory, "objective", "") or "cheapest-that-clears"),
            "prefer_harness": str(getattr(inventory, "prefer_harness", "") or ""),
            # asdict: the verb reads fields by name; a key it ignores is harmless.
            "rows": [dataclasses.asdict(r) for r in inventory.rows.values()],
        }
    except Exception:  # noqa: BLE001 - an unreadable inventory grids on defaults
        return {}


def _thread_seatable(harnesses: list[str]) -> dict[str, bool]:
    try:
        from fno.agents.harness_map import thread_seatable

        return {h: bool(thread_seatable(h)) for h in dict.fromkeys(harnesses)}
    except Exception:  # noqa: BLE001 - unknown harness degrades open
        return {h: True for h in dict.fromkeys(harnesses)}


def _harness_installed_table(harnesses: list[str]) -> dict[str, bool]:
    try:
        from fno.agents.harnesses import READABLE_PROVIDERS

        return {h: h in READABLE_PROVIDERS for h in dict.fromkeys(harnesses)}
    except Exception:  # noqa: BLE001 - an unreadable roster degrades open
        return {h: True for h in dict.fromkeys(harnesses)}


def _effort_ok_table(rows: list[Mapping[str, Any]]) -> dict[str, dict[str, bool]]:
    """Which (harness, effort) pairs survive ``effort_tokens``; the verb only consumes verdicts."""
    out: dict[str, dict[str, bool]] = {}
    for row in rows:
        harness, effort = str(row.get("harness", "") or ""), str(row.get("effort", "") or "")
        if not harness or not effort.strip():
            continue
        try:
            from fno.agents.mux_spawn import effort_tokens

            effort_tokens(harness, effort)
            verdict = True
        except Exception:  # noqa: BLE001 - an unusable effort surface is omitted
            verdict = False
        out.setdefault(harness, {})[effort] = verdict
    return out


def _vendor_tables(settings: object, rows: dict[str, Any], lanes: list[Any]) -> dict[str, Any]:
    """Vendor caps and live counts for every vendor the rows or inline lanes name by ``route``."""
    caps: dict[str, int] = {}
    counts: dict[str, int] = {}
    errors: dict[str, str] = {}
    try:
        from fno.agents.spawn_gate import (
            ProviderCountUnavailable, provider_lanes_cap, provider_live_count,
        )
        from fno.config import provider_limits_table

        table = dict(provider_limits_table(getattr(settings, "agents", None)))
        routes = [str(row.get("route", "") or "") for row in rows.values()]
        routes += [str(lane.get("route", "") or "") for lane in lanes if isinstance(lane, Mapping)]
        vendors = sorted({r.replace(",", "/").partition("/")[0].strip()
                          for r in routes if r.strip()} - {""})
        for vendor in vendors:
            cap = provider_lanes_cap(table.get(vendor))
            if cap is None:
                continue
            caps[vendor] = int(cap)
            try:
                counts[vendor] = int(provider_live_count(vendor))
            except ProviderCountUnavailable as exc:
                errors[vendor] = str(exc)
    except Exception:  # noqa: BLE001 - an unreadable cap table caps no lane
        pass
    return {"vendor_caps": caps, "vendor_counts": counts, "vendor_count_errors": errors}


def _account_record_vendors(settings: object) -> dict[str, str]:
    try:
        return {
            str(r["id"]): str(r.get("route", "") or "").replace(",", "/").partition("/")[0].strip()
            for r in getattr(getattr(settings, "accounts", None), "records", None) or []
            if isinstance(r, Mapping) and r.get("id") and str(r.get("route", "") or "").strip()
        }
    except Exception:  # noqa: BLE001 - an unreadable registry contradicts nothing
        return {}


def _slot_profiles_table(settings: object) -> dict[str, Any]:
    """Every dispatched verb's slot as JSON: the strict owner picks the
    EFFECTIVE work kind's slot from this table, so the request's own verb
    never has to match it (a planless target rides the blueprint slot)."""
    out: dict[str, Any] = {}
    try:
        for verb in SLOT_VERBS:
            _s, prof, lns = _slot_entry(settings, verb)
            if prof is None and not lns:
                continue
            by_diff = getattr(prof, "by_difficulty", None)
            out[verb] = {
                "rung_base": f"agents.profiles.{verb}",
                "profile": _profile_fields(prof),
                "lanes_raw": _lanes_payload(lns) if isinstance(lns, (list, tuple)) else [],
                "has_overlay": isinstance(by_diff, Mapping) and bool(by_diff),
            }
    except Exception:  # noqa: BLE001 - an unreadable table leaves slots unnamed
        return {}
    return out


def _routing_policy_payload(settings: object) -> dict[str, Any]:
    routing = getattr(settings, "routing", None)
    return {
        "enforce_inventory": bool(getattr(routing, "enforce_inventory", False)),
        "operator_access": str(
            getattr(routing, "operator_access", "") or "unknown"
        ).strip().lower(),
    }


def _slot_payload(
    *, rung_base: str, profile: Optional[object], lanes: Any, node: Optional[Mapping],
    capacity: Optional[Mapping[str, object]], inventory: Optional[Any], settings: object,
    substrate: Optional[str], permission_mode: Optional[str], constrain_harness: Optional[str],
    explicit_lane: bool, explicit_model: bool, gate_bypassed: bool,
    role: Optional[str] = None, protected_role: Optional[str] = None,
    model_occupied: bool = False,
    work_verb: Optional[str] = None,
    explicit_model_value: Optional[str] = None,
    explicit_route_value: Optional[str] = None,
    explicit_vendor_value: Optional[str] = None,
) -> dict[str, Any]:
    """The slot/grid payload: both legs' inputs plus the gather the verb cannot do."""
    rows = _declared_rows(settings)
    lanes_payload = _lanes_payload(lanes) if isinstance(lanes, (list, tuple)) else lanes
    inventory_payload = _inventory_payload(inventory)
    inv_rows = inventory_payload.get("rows", [])
    node_payload = None
    if node:
        node_payload = {
            "difficulty": node.get("difficulty"),
            "priority": node.get("priority"),
            # Plan-presence evidence: the work-kind owner reads presence, never
            # plan quality, and needs it even when the model axis is occupied.
            "plan_path": str(node.get("plan_path") or ""),
        }
    payload: dict[str, Any] = {
        "rung_base": rung_base,
        "lanes_raw": lanes_payload,
        "declared_rows": rows,
        "profile": _profile_fields(profile),
        "node": node_payload,
        "capacity": dict(capacity or {}),
        "substrate": substrate,
        "permission_mode": permission_mode,
        "constrain_harness": constrain_harness,
        "explicit_lane": explicit_lane,
        "explicit_model": explicit_model,
        "gate_bypassed": gate_bypassed,
        "thread_seatable": _thread_seatable(
            [str(r.get("harness", "")) for r in rows.values()]
            + [str(r.get("harness", "")) for r in inv_rows]
            + [str(lane.get("provider", "") or "") for lane in (lanes_payload or [])
               if isinstance(lane, Mapping)]
        ),
        "account_record_vendors": _account_record_vendors(settings),
        "role": role,
        "protected_role": protected_role,
        "model_occupied": model_occupied,
        "inventory": inventory_payload,
        "work_verb": work_verb,
        "policy": _routing_policy_payload(settings),
        "slot_by_verb": _slot_profiles_table(settings),
        "explicit_model_value": explicit_model_value,
        "explicit_route_value": explicit_route_value,
        "explicit_vendor_value": explicit_vendor_value,
    }
    try:
        payload["effort_ok"] = _effort_ok_table(inv_rows)
    except Exception:  # noqa: BLE001 - an unusable effort table omits nothing
        payload["effort_ok"] = {}
    try:
        payload["harness_installed"] = _harness_installed_table(
            [str(r.get("harness", "") or "") for r in inv_rows])
    except Exception:  # noqa: BLE001 - an unreadable roster degrades open
        payload["harness_installed"] = {}
    payload.update(_vendor_tables(
        settings, rows, lanes_payload if isinstance(lanes_payload, list) else []))
    return payload


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
    profile = None
    if verb and settings is not None:
        try:
            profiles = getattr(getattr(settings, "agents", None), "profiles", None) or {}
            profile = profiles.get(verb)
        except Exception:  # noqa: BLE001
            profile = None
    lanes = getattr(profile, "lanes", None) if profile is not None else None
    return settings, profile, lanes


def slot_states(
    verb: str,
    capacity: Optional[Mapping[str, object]],
    *,
    inventory: Optional[Inventory] = None,
    settings: object = None,
) -> dict[str, Any]:
    """Readout of one verb's slot: lanes, policy lines, and would_take - the
    verb's own answer. Display, never selection."""
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
    rung_base = f"agents.profiles.{verb}"
    payload = _slot_payload(
        rung_base=rung_base, profile=_profile, lanes=lanes, node=None,
        capacity=capacity, inventory=inventory, settings=settings,
        substrate=None, permission_mode=None, constrain_harness=None,
        explicit_lane=False, explicit_model=False, gate_bypassed=False,
    )
    payload["mode"] = "states"
    states: dict[str, Any] = {}
    try:
        from fno.route_slot_client import route_slot_call

        states = route_slot_call(payload)
    except Exception as exc:  # noqa: BLE001 - a missing verb degrades the readout
        states = {"would_take": f"slot=route-slot-unavailable ({exc})"}
    for key in (
        "on_exhausted", "on_low", "on_unknown", "would_take", "routing",
        "work_kind", "operator_access", "policy_source", "skipped",
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


def harness_accounts(
    harness: str, *, settings: object = None, inventory: Optional[Inventory] = None
) -> list[str]:
    """Expand a harness to the ACCOUNT record ids reachable through it:
    registered records plus inventory-row accounts."""
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
    """proven|mismatch per account, from the attribution owner alone: the
    active slot's record id is ``proven``, any other named account on a proven
    slot is ``mismatch``. No owner answer reads unknown. Never reads
    credentials itself."""
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
    """Harness capacity: per-account headroom aggregated MAX (exhausted only
    if EVERY account is); a proven active slot account IS the aggregate. The
    value is ``{state, window, accounts, evidence, resets}``. Never probes,
    never touches the network.
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
    """Resolve a tier to a concrete declared model, scoped to one harness when
    asked. The band math lives on ``fno-agents route-slot``; never raises."""
    from fno.route_slot_client import RouteSlotUnavailable, route_slot_call

    inv = inventory if inventory is not None else resolve_inventory(
        settings=settings, snapshot=snapshot
    )
    try:
        return _answer(route_slot_call(
            {"mode": "tier", "tier": tier, "provider": provider,
             "inventory": _inventory_payload(inv)}), "model")
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
    """Apply the full precedence chain; ``(model, decision_source, chain)``.
    Pins bypass the band filter - operator authority outranks routing."""
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
    """Concrete ``--model`` for a node at the spawn seam, or None for default.
    Strictly non-fatal: an error degrades to the explicit override or the
    node's raw pin."""
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
