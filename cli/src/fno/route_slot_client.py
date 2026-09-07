"""The spawn seam's client for the ``fno-agents route-slot`` verb.

The slot resolver is Rust (law d-450caaeb: no net-new Python feature; verbs
port as they are touched). This module is the Python adapter: it gathers one
JSON payload (profile fields, raw lanes, declared routing rows, the capacity
snapshot, posture flags, vendor counts/caps, per-record vendors) and hands the
JSON answer back untouched. The chain strings are the receipt vocabulary the
seam, advance and the doctor match on; they come from Rust verbatim and are
never reworded here. The capacity snapshot and live counts stay Python-side
input adapters over the attribution owners; the verb never reads state.
"""
from __future__ import annotations

import json
import subprocess
from typing import Any, Mapping, Optional

from fno.rust_binary import find_dev_binary, resolve_binary


class RouteSlotUnavailable(RuntimeError):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def _profile_fields(profile: Optional[object]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for name in ("on_exhausted", "on_low", "on_unknown"):
        out[name] = str(getattr(profile, name, "") or "")
    by_diff = getattr(profile, "by_difficulty", None)
    out["by_difficulty"] = by_diff if isinstance(by_diff, Mapping) else {}
    return out


def _declared_rows(settings: object) -> dict[str, Any]:
    """The CONFIG-declared rows exactly (never the built-in fallback): the
    pre-port fold read ``settings.routing.models`` and the slot walks only
    rows the operator named."""
    rows: dict[str, Any] = {}
    try:
        for row in getattr(getattr(settings, "routing", None), "models", None) or []:
            if isinstance(row, Mapping):
                name = str(row.get("name", "") or "").strip()
                if name:
                    rows[name] = {
                        "name": name,
                        "harness": str(row.get("harness", "") or "").strip(),
                        "model": str(row.get("model", "") or "").strip(),
                        "route": str(row.get("route", "") or "").strip(),
                        "account": str(row.get("account", "") or "").strip(),
                        "band": str(row.get("band", "") or "").strip(),
                        "effort": str(row.get("effort", "") or "").strip(),
                    }
    except Exception:  # noqa: BLE001 - an unreadable config reads as empty
        return {}
    return rows


def _thread_seatable(harnesses: list[str]) -> dict[str, bool]:
    out: dict[str, bool] = {}
    for harness in dict.fromkeys(harnesses):
        try:
            from fno.agents.harness_map import thread_seatable

            out[harness] = bool(thread_seatable(harness))
        except Exception:  # noqa: BLE001 - unknown harness degrades open
            continue
    return out


def _vendor_tables(
    settings: object, rows: dict[str, Any], lanes: list[Any]
) -> dict[str, Any]:
    """Vendor caps and live counts, gathered once for the whole lane list.
    Vendors come from BOTH spellings: declared rows and inline lane tables
    (an inline lane's own ``route`` names its vendor)."""
    caps: dict[str, int] = {}
    counts: dict[str, int] = {}
    errors: dict[str, str] = {}
    try:
        from fno.agents.spawn_gate import (
            ProviderCountUnavailable,
            provider_lanes_cap,
            provider_live_count,
        )
        from fno.config import provider_limits_table

        table = dict(provider_limits_table(getattr(settings, "agents", None)))
        routes = [str(row.get("route", "") or "") for row in rows.values()]
        routes += [
            str(lane.get("route", "") or "")
            for lane in lanes
            if isinstance(lane, Mapping)
        ]
        vendors = sorted({
            route.replace(",", "/").partition("/")[0].strip()
            for route in routes
            if route.strip()
        } - {""})
        for vendor in vendors:
            cap = provider_lanes_cap(table.get(vendor))
            if cap is not None:
                caps[vendor] = int(cap)
                try:
                    counts[vendor] = int(provider_live_count(vendor))
                except ProviderCountUnavailable as exc:
                    errors[vendor] = str(exc)
    except Exception:  # noqa: BLE001 - an unreadable cap table caps no lane
        return {"vendor_caps": caps, "vendor_counts": counts, "vendor_count_errors": errors}
    return {"vendor_caps": caps, "vendor_counts": counts, "vendor_count_errors": errors}


def _account_record_vendors(settings: object) -> dict[str, str]:
    out: dict[str, str] = {}
    try:
        records = getattr(getattr(settings, "accounts", None), "records", None) or []
        for record in records:
            if isinstance(record, Mapping):
                route = str(record.get("route", "") or "")
                vendor = route.replace(",", "/").partition("/")[0].strip()
                if record.get("id") and vendor:
                    out[str(record["id"])] = vendor
    except Exception:  # noqa: BLE001 - an unreadable registry contradicts nothing
        return {}
    return out


def _lanes_payload(lanes: Any) -> list[Any]:
    """Lane entries as JSON: dicts pass through; pre-built profile lane
    objects serialize by their known attributes (their vocabulary is the same
    ``SLOT_LANE_FIELDS`` table the verb validates against)."""
    fields = (
        "provider", "model", "effort", "substrate", "permission_mode",
        "route", "account", "pane_group",
    )
    out: list[Any] = []
    for lane in lanes or []:
        if isinstance(lane, Mapping):
            out.append(dict(lane))
        elif hasattr(lane, "provider") or hasattr(lane, "model"):
            out.append({k: str(getattr(lane, k, "") or "") for k in fields})
        else:
            out.append(lane)
    return out


def _inventory_payload(inventory: Optional[Any]) -> dict[str, Any]:
    """The resolved inventory as JSON: rows in declared order, the objective,
    the prefer-harness tiebreaker, and whether CONFIG named anything."""
    if inventory is None:
        return {}
    try:
        rows = [
            {
                "name": r.name,
                "harness": r.harness,
                "model": r.model,
                "route": r.route,
                "account": r.account,
                "band": r.band,
                "percentile": r.percentile,
                "effort": r.effort,
                "cost_per_mtok_in": r.cost_per_mtok_in,
            }
            for r in inventory.rows.values()
        ]
        return {
            "declared": bool(getattr(inventory, "declared", False)),
            "objective": str(getattr(inventory, "objective", "") or "cheapest-that-clears"),
            "prefer_harness": str(getattr(inventory, "prefer_harness", "") or ""),
            "rows": rows,
        }
    except Exception:  # noqa: BLE001 - an unreadable inventory grids on defaults
        return {}


def _harness_installed_table(harnesses: list[str]) -> dict[str, bool]:
    out: dict[str, bool] = {}
    for harness in dict.fromkeys(harnesses):
        try:
            from fno.agents.harnesses import READABLE_PROVIDERS

            out[harness] = harness in READABLE_PROVIDERS
        except Exception:  # noqa: BLE001 - an unreadable roster degrades open
            out[harness] = True
    return out


def _effort_ok_table(rows: list[Mapping[str, Any]]) -> dict[str, dict[str, bool]]:
    """Which (harness, effort) pairs survive ``effort_tokens``: the vocabulary
    stays owned by the harness surface code; the verb only consumes verdicts."""
    out: dict[str, dict[str, bool]] = {}
    pairs = {
        (str(r.get("harness", "") or ""), str(r.get("effort", "") or ""))
        for r in rows
        if str(r.get("effort", "") or "").strip()
    }
    for harness, effort in pairs:
        if not harness:
            continue
        try:
            from fno.agents.mux_spawn import effort_tokens

            effort_tokens(harness, effort)
            out.setdefault(harness, {})[effort] = True
        except Exception:  # noqa: BLE001 - an unusable effort surface is omitted
            out.setdefault(harness, {})[effort] = False
    return out


def resolve_slot_via_binary(
    *,
    rung_base: str,
    profile: Optional[object],
    lanes: Any,
    node: Optional[Mapping],
    capacity: Optional[Mapping[str, object]],
    inventory: Optional[Any],
    settings: object,
    substrate: Optional[str],
    permission_mode: Optional[str],
    constrain_harness: Optional[str],
    explicit_lane: bool,
    explicit_model: bool,
    gate_bypassed: bool,
    role: Optional[str] = None,
    protected_role: Optional[str] = None,
    model_occupied: bool = False,
) -> tuple[Optional[dict], list[str]]:
    """Call ``fno-agents route-slot`` and return its ``(candidate, chain)``.

    The payload carries BOTH legs: the slot walk (lanes, policies, posture)
    and the grid inputs (node difficulty/priority, role, the resolved
    inventory) so the verb answers the one-slot-or-grid question in one call.
    Raises :class:`RouteSlotUnavailable` when the binary is missing, fails,
    or answers malformed JSON; the caller turns that into a named refusal
    rather than a silent lane-less spawn.
    """
    import os

    # A dev checkout's own build outranks any installed copy: testing against
    # a stale PATH binary would resolve lanes with last release's vocabulary.
    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        raise RouteSlotUnavailable(
            "the fno-agents binary was not found; reinstall fno,"
            " run `fno doctor update --rust`, or set FNO_AGENTS_BIN"
        )
    rows = _declared_rows(settings)
    lanes_payload = _lanes_payload(lanes) if isinstance(lanes, (list, tuple)) else lanes
    inv_rows = []
    try:
        inv_rows = [
            {"harness": r.harness} for r in (inventory.rows.values() if inventory else [])
        ]
    except Exception:  # noqa: BLE001 - an unreadable inventory degrades open
        inv_rows = []
    seatable = _thread_seatable(
        [str(r.get("harness", "")) for r in rows.values()]
        + [str(r.get("harness", "")) for r in inv_rows]
        + [
            str(lane.get("provider", "") or "")
            for lane in (lanes_payload or [])
            if isinstance(lane, Mapping)
        ]
    )
    payload: dict[str, Any] = {
        "rung_base": rung_base,
        "lanes_raw": lanes_payload,
        "declared_rows": rows,
        "profile": _profile_fields(profile),
        "node": {"difficulty": (node or {}).get("difficulty"),
                 "priority": (node or {}).get("priority")} if node else None,
        "capacity": dict(capacity or {}),
        "substrate": substrate,
        "permission_mode": permission_mode,
        "constrain_harness": constrain_harness,
        "explicit_lane": explicit_lane,
        "explicit_model": explicit_model,
        "gate_bypassed": gate_bypassed,
        "thread_seatable": seatable,
        "account_record_vendors": _account_record_vendors(settings),
        "role": role,
        "protected_role": protected_role,
        "model_occupied": model_occupied,
        "inventory": _inventory_payload(inventory),
    }
    try:
        payload["effort_ok"] = _effort_ok_table(
            payload["inventory"].get("rows", [])
        )
    except Exception:  # noqa: BLE001 - an unusable effort table omits nothing
        payload["effort_ok"] = {}
    try:
        payload["harness_installed"] = _harness_installed_table([
            str(r.get("harness", "") or "")
            for r in payload["inventory"].get("rows", [])
        ])
    except Exception:  # noqa: BLE001 - an unreadable roster degrades open
        payload["harness_installed"] = {}
    payload.update(
        _vendor_tables(
            settings, rows, lanes_payload if isinstance(lanes_payload, list) else []
        )
    )
    out = _route_slot_call(binary, payload)
    return out.get("candidate"), [str(line) for line in (out.get("chain") or [])]


def _route_slot_call(binary: object, payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip: JSON payload in, parsed JSON answer out."""
    import os

    try:
        proc = subprocess.run(
            [str(binary), "route-slot"],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RouteSlotUnavailable(f"fno-agents route-slot failed: {exc}") from exc
    if proc.returncode != 0:
        raise RouteSlotUnavailable(
            f"fno-agents route-slot exited {proc.returncode}: {proc.stderr.strip()[:200]}"
        )
    try:
        out = json.loads(proc.stdout)
    except ValueError as exc:
        raise RouteSlotUnavailable(f"fno-agents route-slot bad output: {exc}") from exc
    if os.environ.get("FNO_ROUTE_SLOT_DEBUG"):
        print(json.dumps({"payload": payload, "out": out}), flush=True)
    return out


def route_tier_via_binary(
    tier: Optional[str],
    provider: Optional[str],
    inventory: Optional[Any],
) -> tuple[Optional[str], list[str]]:
    """Tier resolution on the verb: returns ``(model, chain)``."""
    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        raise RouteSlotUnavailable(
            "the fno-agents binary was not found; reinstall fno,"
            " run `fno doctor update --rust`, or set FNO_AGENTS_BIN"
        )
    payload = {"mode": "tier", "tier": tier, "provider": provider,
               "inventory": _inventory_payload(inventory)}
    out = _route_slot_call(binary, payload)
    return out.get("model"), [str(line) for line in (out.get("chain") or [])]


def route_states_via_binary(
    *,
    rung_base: str,
    profile: Optional[object],
    lanes: Any,
    node: Optional[Mapping],
    capacity: Optional[Mapping[str, object]],
    settings: object,
) -> tuple[list[dict], list[str]]:
    """Per-lane capacity states for the readout: returns ``(states, chain)``."""
    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        raise RouteSlotUnavailable(
            "the fno-agents binary was not found; reinstall fno,"
            " run `fno doctor update --rust`, or set FNO_AGENTS_BIN"
        )
    rows = _declared_rows(settings)
    lanes_payload = _lanes_payload(lanes) if isinstance(lanes, (list, tuple)) else lanes
    payload = {
        "mode": "states",
        "rung_base": rung_base,
        "lanes_raw": lanes_payload,
        "declared_rows": rows,
        "profile": _profile_fields(profile),
        "node": {"difficulty": (node or {}).get("difficulty")} if node else None,
        "capacity": dict(capacity or {}),
    }
    out = _route_slot_call(binary, payload)
    return out.get("lane_states") or [], [str(line) for line in (out.get("chain") or [])]
