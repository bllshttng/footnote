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

from fno.rust_binary import resolve_binary


class RouteSlotUnavailable(RuntimeError):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def _profile_fields(profile: Optional[object]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for name in ("on_exhausted", "on_low", "on_unknown"):
        out[name] = str(getattr(profile, name, "") or "")
    by_diff = getattr(profile, "by_difficulty", None)
    out["by_difficulty"] = by_diff if isinstance(by_diff, Mapping) else {}
    return out


def _declared_rows(inventory: Optional[Any]) -> dict[str, Any]:
    rows: dict[str, Any] = {}
    if inventory is None:
        return rows
    try:
        for name, row in inventory.rows.items():
            rows[name] = {
                "name": name,
                "harness": row.harness,
                "model": row.model,
                "route": row.route,
                "account": row.account,
                "band": row.band,
                "effort": row.effort,
            }
    except Exception:  # noqa: BLE001 - an unreadable inventory reads as empty
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


def _vendor_tables(settings: object, rows: dict[str, Any]) -> dict[str, Any]:
    """Vendor caps and live counts, gathered once for the whole lane list."""
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
        vendors = sorted({
            str(row.get("route", "") or "").replace(",", "/").partition("/")[0].strip()
            for row in rows.values()
            if str(row.get("route", "") or "").strip()
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
) -> tuple[Optional[dict], list[str]]:
    """Call ``fno-agents route-slot`` and return its ``(candidate, chain)``.

    Raises :class:`RouteSlotUnavailable` when the binary is missing, fails, or
    answers malformed JSON; the caller turns that into a named refusal rather
    than a silent lane-less spawn.
    """
    import os

    binary = resolve_binary()
    if binary is None:
        raise RouteSlotUnavailable(
            "the fno-agents binary was not found; reinstall fno,"
            " run `fno doctor update --rust`, or set FNO_AGENTS_BIN"
        )
    rows = _declared_rows(inventory)
    seatable = _thread_seatable([str(r.get("harness", "")) for r in rows.values()])
    payload: dict[str, Any] = {
        "rung_base": rung_base,
        "lanes_raw": lanes if isinstance(lanes, (list, tuple)) else [],
        "declared_rows": rows,
        "profile": _profile_fields(profile),
        "node": {"difficulty": (node or {}).get("difficulty")} if node else None,
        "capacity": dict(capacity or {}),
        "substrate": substrate,
        "permission_mode": permission_mode,
        "constrain_harness": constrain_harness,
        "explicit_lane": explicit_lane,
        "explicit_model": explicit_model,
        "gate_bypassed": gate_bypassed,
        "thread_seatable": seatable,
        "account_record_vendors": _account_record_vendors(settings),
    }
    payload.update(_vendor_tables(settings, rows))
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
        candidate = out.get("candidate")
        chain = out.get("chain")
    except (ValueError, AttributeError) as exc:
        raise RouteSlotUnavailable(f"fno-agents route-slot bad output: {exc}") from exc
    if os.environ.get("FNO_ROUTE_SLOT_DEBUG"):
        print(json.dumps({"payload": payload, "out": out}), flush=True)
    return candidate, [str(line) for line in (chain or [])]
