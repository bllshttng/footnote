"""Read-only planning for reusing one mux worker on its existing node."""
from __future__ import annotations

import io
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Mapping, Optional, Sequence

from fno.agents.harness_map import dispatch_command
from fno.agents.registry import AgentEntry, resolve_agent
from fno.agents.spawn_defaults import inject_spawn_defaults


@dataclass(frozen=True)
class RetaskCoordinate:
    harness: str
    provider: Optional[str]
    model: Optional[str]
    effort: Optional[str]
    substrate: str
    permission_mode: Optional[str]
    route: Optional[str]
    account: Optional[str]


def _flag_value(args: Sequence[str], *names: str) -> Optional[str]:
    for index, token in enumerate(args):
        for name in names:
            if token == name and index + 1 < len(args):
                return args[index + 1]
            if token.startswith(f"{name}="):
                return token.split("=", 1)[1]
    return None


def resolve_target_coordinate(
    node: str,
    *,
    settings: object = None,
    model: Optional[str] = None,
    effort: Optional[str] = None,
    env: Optional[Mapping[str, str]] = None,
) -> RetaskCoordinate:
    args = ["spawn", "--name", "retask-probe"]
    if model is not None:
        args += ["--model", model]
    if effort is not None:
        args += ["--effort", effort]
    args.append(f"/fno:target {node}")
    resolved = inject_spawn_defaults(
        args,
        settings=settings,
        env=env,
        stderr=io.StringIO(),
    )
    harness = _flag_value(resolved, "--harness", "-H")
    if not harness:
        from fno.dispatch_flags import resolve_dispatch_provider

        harness = resolve_dispatch_provider(None, env=env)[0]
    route = _flag_value(resolved, "--route")
    provider = _flag_value(resolved, "--provider", "-P")
    route_model: Optional[str] = None
    if route:
        provider, separator, route_model = route.replace(",", "/").partition("/")
        if not separator:
            provider = route
            route_model = None
    resolved_model = _flag_value(resolved, "--model", "-m") or route_model
    return RetaskCoordinate(
        harness=harness,
        provider=provider,
        model=resolved_model,
        effort=_flag_value(resolved, "--effort"),
        substrate=_flag_value(resolved, "--substrate") or "pane",
        permission_mode=_flag_value(resolved, "--permission-mode"),
        route=route,
        account=_flag_value(resolved, "--account"),
    )


def detect_retask(
    entry: AgentEntry,
    target: RetaskCoordinate,
    *,
    node: str,
) -> dict:
    if entry.status != "live":
        return {"outcome": "refused", "reason": "worker_not_live"}
    mux = entry.mux if isinstance(entry.mux, dict) else None
    if not mux or not mux.get("session") or not mux.get("pane_id"):
        return {"outcome": "refused", "reason": "worker_has_no_mux_ref"}
    if not entry.harness_session_id:
        return {"outcome": "refused", "reason": "worker_has_no_session_id"}

    if target.permission_mode is not None:
        return {"outcome": "spawn_required", "reason": "permission_mode"}
    if target.account is not None:
        return {"outcome": "spawn_required", "reason": "account"}
    current_axes = {
        "harness": entry.harness,
        "provider": entry.provider,
        "substrate": "pane",
    }
    target_axes = {
        "harness": target.harness,
        "provider": target.provider,
        "substrate": target.substrate,
    }
    for axis in ("harness", "provider", "substrate"):
        if current_axes[axis] != target_axes[axis]:
            return {"outcome": "spawn_required", "reason": axis}

    desired_model = target.model or entry.model
    desired_effort = target.effort or entry.effort
    switch_required = (entry.model, entry.effort) != (desired_model, desired_effort)
    switch: dict[str, object] = {"required": False}
    if switch_required:
        switch = {
            "required": True,
            "from": {"model": entry.model, "effort": entry.effort},
            "to": {"model": desired_model, "effort": desired_effort},
            "mechanism": "pending_operator_decision",
        }
    payload = {
        "schema_version": 1,
        "worker": entry.name,
        "source_session_id": entry.harness_session_id,
        "mux": {"session": mux["session"], "pane_id": mux["pane_id"]},
        "node": node,
        "target": asdict(target),
        "target_command": dispatch_command(target.harness).format(id=node),
        "switch": switch,
        "execution": {"mode": "read_only_plan"},
        "preconditions": [
            "positive_ready_marker",
            "changed_session_id_after_clear",
            "verified_target_tier_before_submit",
        ],
    }
    return {
        "outcome": "switch_pending" if switch_required else "retask_ready",
        "payload": payload,
    }


def plan_retask(
    worker: str,
    *,
    node: str,
    settings: object = None,
    model: Optional[str] = None,
    effort: Optional[str] = None,
    env: Optional[Mapping[str, str]] = None,
    registry_path: Optional[Path] = None,
) -> dict:
    entry = resolve_agent(worker, path=registry_path).entry
    target = resolve_target_coordinate(
        node,
        settings=settings,
        model=model,
        effort=effort,
        env=env,
    )
    return detect_retask(entry, target, node=node)
