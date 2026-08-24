"""Read-only planning for reusing one mux worker on its existing node."""
from __future__ import annotations

import io
import re
import subprocess
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Mapping, Optional, Sequence

from fno.agents.harness_map import capabilities, dispatch_command
from fno.agents.registry import (
    AgentEntry,
    AgentResolutionError,
    load_registry,
    rename_agent,
    resolve_agent,
)
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


def _status_tier(harness: str, frame: str) -> Optional[dict[str, str]]:
    strategy = capabilities(harness)["model_switch_strategy"]
    pattern = strategy.get("status_pattern") or ""
    match = re.search(pattern, frame)
    if not match or not match.groupdict().get("model") or not match.groupdict().get("effort"):
        return None
    return {"model": match.group("model"), "effort": match.group("effort")}


def _menu_delta(frame: str, target: str) -> Optional[int]:
    rows: list[tuple[int, bool, str]] = []
    for line in frame.splitlines():
        match = re.match(r"^\s*(?P<cursor>›\s*)?(?P<row>\d+)\.\s+(?P<label>.*)$", line)
        if match:
            rows.append((int(match.group("row")), bool(match.group("cursor")), match.group("label")))
    current = next((row for row, cursor, _label in rows if cursor), None)
    desired = next((row for row, _cursor, label in rows if target in label), None)
    if current is None or desired is None:
        return None
    return desired - current


def _walk_menu(
    frame: str,
    *,
    target: str,
    send: Callable[[str, bool], bool],
    read_frame: Callable[[], str],
) -> bool:
    delta = _menu_delta(frame, target)
    if delta is None:
        return False
    arrow = "\x1b[B" if delta > 0 else "\x1b[A"
    for _ in range(abs(delta)):
        if not send(arrow, False):
            return False
    verified = read_frame()
    if _menu_delta(verified, target) != 0:
        return False
    return send("", True)


def execute_retask(
    entry: AgentEntry,
    target: RetaskCoordinate,
    *,
    node: str,
    read_frame: Callable[[], str],
    send: Callable[[str, bool], bool],
    restamp: Callable[[], Optional[str]],
    rename: Callable[[str], Optional[str]],
    project_tier: Callable[[str, str], None] = lambda _model, _effort: None,
) -> dict:
    """Run the bounded retask transaction through injected pane seams."""
    refusal = {
        "status": "refused",
        "cleared": False,
        "session_restamped": False,
        "switch": "not_started",
        "switch_verified": False,
        "target_submit_confirmed": False,
    }
    planned = detect_retask(entry, target, node=node)
    if planned["outcome"] in {"spawn_required", "refused"}:
        return {**refusal, "reason": planned.get("reason", planned["outcome"])}
    strategy = capabilities(entry.harness)["model_switch_strategy"]
    desired_model = target.model or entry.model
    desired_effort = target.effort or entry.effort
    if (
        strategy["kind"] == "unsupported"
        and (entry.model, entry.effort) != (desired_model, desired_effort)
    ):
        return {**refusal, "reason": "unsupported_switch_strategy"}
    screen = entry.screen_state or {}
    ready_marker = capabilities(entry.harness)["ready_marker"]
    if screen.get("state") != "idle" or screen.get("rule") != ready_marker:
        return {**refusal, "reason": "pane_not_idle"}
    if not read_frame().strip():
        return {**refusal, "reason": "pane_frame_unreadable"}
    if not send("/clear", True):
        return {**refusal, "reason": "clear_not_confirmed"}
    new_session = restamp()
    if not new_session or new_session == entry.harness_session_id:
        return {**refusal, "cleared": True, "reason": "session_not_restamped"}
    renamed = rename(f"target-{node}")
    if not renamed:
        return {
            **refusal,
            "cleared": True,
            "session_restamped": True,
            "reason": "registry_rename_refused",
        }
    if strategy["kind"] == "unsupported":
        return {
            **refusal,
            "cleared": True,
            "session_restamped": True,
            "registry_name": renamed,
            "reason": "unsupported_switch_strategy",
        }
    status_command = strategy["status_command"]
    if not send(status_command, True):
        return {
            **refusal,
            "cleared": True,
            "session_restamped": True,
            "registry_name": renamed,
            "reason": "status_not_confirmed",
        }
    cleared_tier = _status_tier(entry.harness, read_frame())
    if cleared_tier is None:
        return {
            **refusal,
            "cleared": True,
            "session_restamped": True,
            "registry_name": renamed,
            "reason": "status_unreadable",
        }
    try:
        project_tier(cleared_tier["model"], cleared_tier["effort"])
    except (OSError, RuntimeError, ValueError):
        return {
            **refusal,
            "cleared": True,
            "session_restamped": True,
            "registry_name": renamed,
            "reason": "registry_projection_failed",
        }
    switch_needed = (
        cleared_tier["model"] != desired_model or cleared_tier["effort"] != desired_effort
    )
    if not switch_needed:
        switch = "skipped_same_tier"
    elif strategy["kind"] == "direct":
        for template in strategy["tokens"]:
            command = template.format(model=desired_model or "", effort=desired_effort or "")
            if not send(command, True):
                return {
                    **refusal,
                    "cleared": True,
                    "session_restamped": True,
                    "registry_name": renamed,
                    "reason": "switch_not_confirmed",
                }
        switch = "switched"
    else:
        tokens = strategy["tokens"]
        if not send(tokens[0], True):
            return {
                **refusal,
                "cleared": True,
                "session_restamped": True,
                "registry_name": renamed,
                "reason": "switch_not_confirmed",
            }
        if not _walk_menu(
            read_frame(), target=desired_model or "", send=send, read_frame=read_frame
        ):
            return {
                **refusal,
                "cleared": True,
                "session_restamped": True,
                "registry_name": renamed,
                "reason": "model_row_missing",
            }
        effort_label = strategy["effort_labels"].get(desired_effort or "")
        if not effort_label:
            return {
                **refusal,
                "cleared": True,
                "session_restamped": True,
                "registry_name": renamed,
                "reason": "effort_label_missing",
            }
        if not _walk_menu(
            read_frame(), target=effort_label, send=send, read_frame=read_frame
        ):
            return {
                **refusal,
                "cleared": True,
                "session_restamped": True,
                "registry_name": renamed,
                "reason": "effort_row_missing",
            }
        switch = "switched"
    if switch == "switched":
        if not send(status_command, True):
            return {
                **refusal,
                "cleared": True,
                "session_restamped": True,
                "registry_name": renamed,
                "switch": switch,
                "reason": "post_switch_status_not_confirmed",
            }
        verified = _status_tier(entry.harness, read_frame())
        if verified != {"model": desired_model, "effort": desired_effort}:
            return {
                **refusal,
                "cleared": True,
                "session_restamped": True,
                "registry_name": renamed,
                "switch": switch,
                "reason": "post_switch_status_mismatch",
            }
        try:
            project_tier(verified["model"], verified["effort"])
        except (OSError, RuntimeError, ValueError):
            return {
                **refusal,
                "cleared": True,
                "session_restamped": True,
                "registry_name": renamed,
                "switch": switch,
                "reason": "registry_projection_failed",
            }
    target_command = planned["payload"]["target_command"]
    submitted = send(target_command, True)
    if not submitted:
        return {
            **refusal,
            "cleared": True,
            "session_restamped": True,
            "registry_name": renamed,
            "switch": switch,
            "switch_verified": switch == "skipped_same_tier" or switch == "switched",
            "reason": "target_submit_not_confirmed",
        }
    return {
        "status": "retasked",
        "cleared": True,
        "session_restamped": True,
        "switch": switch,
        "switch_verified": True,
        "target_submit_confirmed": True,
        "registry_name": renamed,
    }


def run_retask(
    worker: str,
    *,
    node: str,
    settings: object = None,
    model: Optional[str] = None,
    effort: Optional[str] = None,
    env: Optional[Mapping[str, str]] = None,
    registry_path: Optional[Path] = None,
) -> dict:
    """Resolve live seams and execute one retask transaction."""
    entry = resolve_agent(worker, path=registry_path).entry
    target = resolve_target_coordinate(
        node,
        settings=settings,
        model=model,
        effort=effort,
        env=env,
    )
    mux = entry.mux or {}
    session = str(mux.get("session"))
    pane = str(mux.get("pane_id"))
    renamed_name = [entry.name]
    restamped_session = [entry.harness_session_id]

    def read_frame() -> str:
        result = subprocess.run(
            ["fno", "mux", "pane", "read", "--session", session, pane, "--lines", "80"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        return result.stdout if result.returncode == 0 else ""

    def send(text: str, submit: bool) -> bool:
        command = [
            "fno", "mux", "pane", "send", "--session", session, pane,
            "--text", text, "--raw",
        ]
        if submit:
            command.append("--submit")
        result = subprocess.run(command, capture_output=True, text=True, timeout=15, check=False)
        return result.returncode == 0

    def restamp() -> Optional[str]:
        for _ in range(40):
            for candidate in load_registry(path=registry_path):
                if candidate.name == entry.name and candidate.harness_session_id:
                    if candidate.harness_session_id != entry.harness_session_id:
                        restamped_session[0] = candidate.harness_session_id
                        return candidate.harness_session_id
            time.sleep(0.25)
        return None

    def rename(new_name: str) -> Optional[str]:
        try:
            renamed_name[0] = rename_agent(
                entry.name, new_name, registry_path=registry_path
            ).name
            return renamed_name[0]
        except (AgentResolutionError, ValueError):
            return None

    def project_tier(model_value: str, effort_value: str) -> None:
        from fno.agents.registry import project_verified_tier

        project_verified_tier(
            renamed_name[0],
            restamped_session[0] or "",
            model=model_value,
            effort=effort_value,
            registry_path=registry_path,
        )

    return execute_retask(
        entry,
        target,
        node=node,
        read_frame=read_frame,
        send=send,
        restamp=restamp,
        rename=rename,
        project_tier=project_tier,
    )


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
