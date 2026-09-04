"""Read-only planning for reusing one mux worker on its existing node."""
from __future__ import annotations

import json
import io
import os
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
    classify_session_transition,
    load_registry,
    rename_agent,
    resolve_agent,
)
from fno.agents.spawn_defaults import inject_spawn_defaults


class RetaskTransportError(RuntimeError):
    """A pane read or send exceeded its bounded transport timeout."""


@dataclass(frozen=True)
class RetaskCoordinate:
    harness: str
    provider: Optional[str]
    model: Optional[str]
    effort: Optional[str]
    substrate: Optional[str]
    permission_mode: Optional[str]
    route: Optional[str]
    account: Optional[str]


def _resolve_retask_node(node: str) -> str:
    """Return the canonical graph id for a retask destination or refuse."""
    from fno.graph._constants import is_wellformed_node_id

    if is_wellformed_node_id(node):
        return node
    from fno.graph.fuzzy import resolve_node
    from fno.graph.load import load_graph

    match = resolve_node(node, load_graph())
    if match.kind != "exact" or not match.id:
        raise ValueError(f"retask node {node!r} does not resolve to a graph id")
    return match.id


def _source_node_for_entry(entry: AgentEntry) -> tuple[Optional[dict], Optional[str]]:
    """Join a live registry row to exactly one graph node."""
    from fno.graph.load import load_graph

    session_id = entry.harness_session_id
    if not session_id:
        return None, "source_node_unresolved"
    by_node: dict = {}
    for node in load_graph():
        for session in node.get("sessions", []):
            if (
                isinstance(session, dict)
                and session.get("harness") == entry.harness
                and session.get("session_id") == session_id
            ):
                by_node[node.get("id")] = node
                break
    matches = list(by_node.values())
    if not matches:
        return None, "source_node_unresolved"
    if len(matches) > 1:
        return None, "source_node_ambiguous"
    return matches[0], None


def _source_preflight(entry: AgentEntry) -> dict:
    """Return a positive source/PR authorization before any pane mutation."""
    try:
        source, reason = _source_node_for_entry(entry)
    except Exception as exc:  # unreadable graph evidence cannot authorize clear
        return {"status": "refused", "reason": "source_node_unresolved", "error": str(exc)}
    if source is None:
        return {"status": "refused", "reason": reason or "source_node_unresolved"}

    pr_number = source.get("pr_number")
    if pr_number is None:
        return {"status": "ready", "source_node_id": source.get("id")}

    try:
        result = subprocess.run(
            ["fno", "do", "pr", "status", str(pr_number), "--refresh"],
            cwd=source.get("cwd") or None,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        payload = json.loads(result.stdout.strip().splitlines()[-1])
    except (OSError, ValueError, IndexError, subprocess.TimeoutExpired) as exc:
        return {
            "status": "refused",
            "reason": "source_pr_status_unknown",
            "source_node_id": source.get("id"),
            "pr": pr_number,
            "error": str(exc),
        }

    state = str(payload.get("pr_state") or payload.get("state") or "").upper()
    if state in {"MERGED", "CLOSED"} or (state == "OPEN" and payload.get("green") is True):
        return {
            "status": "ready",
            "source_node_id": source.get("id"),
            "source_pr": pr_number,
            "pr_state": state,
        }
    if state == "OPEN":
        return {
            "status": "refused",
            "reason": "source_pr_not_green",
            "source_node_id": source.get("id"),
            "pr": pr_number,
            "head": payload.get("head_sha") or payload.get("head"),
            "verdict": payload.get("verdict"),
            "blockers": payload.get("checks"),
        }
    return {
        "status": "refused",
        "reason": "source_pr_status_unknown",
        "source_node_id": source.get("id"),
        "pr": pr_number,
        "verdict": payload.get("verdict"),
    }


def _transition_receipt(value: object, predecessor: str) -> Optional[dict]:
    """Normalize x-dfe7's structured transition receipt at the consumer seam."""
    if isinstance(value, str):
        if value == predecessor:
            return None
        return {
            "classification": "succession",
            "predecessor_session_id": predecessor,
            "current_session_id": value,
            "registry_rows": 1,
            "lineage_recorded": True,
        }
    if not isinstance(value, Mapping):
        return None
    return dict(value)


def _flag_value(args: Sequence[str], *names: str) -> Optional[str]:
    for index, token in enumerate(args):
        for name in names:
            if token == name and index + 1 < len(args):
                return args[index + 1]
            if token.startswith(f"{name}="):
                return token.split("=", 1)[1]
    return None


def resolve_thread_viewport(
    entry: AgentEntry,
    *,
    runner: Optional[Callable[..., subprocess.CompletedProcess[str]]] = None,
) -> tuple[str, int]:
    """Open a thread's dedicated viewport and return its positive pane id."""
    runner = runner or subprocess.run
    thread_id = entry.fno_id
    session = (os.environ.get("FNO_SESSION") or "main").strip()
    if not isinstance(thread_id, str) or not thread_id.strip():
        raise RetaskTransportError("thread_ref_unreadable")
    if not session:
        raise RetaskTransportError("thread_view_unavailable")
    fno_bin = os.environ.get("FNO_BIN") or "fno"

    def invoke(args: list[str], timeout: int) -> subprocess.CompletedProcess[str]:
        try:
            return runner([fno_bin, *args], capture_output=True, text=True, timeout=timeout, check=False)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise RetaskTransportError("thread_view_open_timeout") from exc

    if invoke(["mux", "thread", "--session", session, thread_id], 30).returncode:
        raise RetaskTransportError("thread_view_unavailable")
    # The pane opened above stays open on a join miss and its name stamping
    # can lag the open, so the join retries; the miss names the opened pane.
    for _ in range(3):
        try:
            panes = invoke(["mux", "pane", "ls", "--session", session, "--json"], 10)
            rows = json.loads(panes.stdout)
        except RetaskTransportError:
            raise
        except ValueError as exc:
            raise RetaskTransportError("thread_ref_unreadable") from exc
        matches = [
            row
            for row in rows
            if isinstance(row, Mapping)
            and row.get("name") == entry.name
            and row.get("fno_id") == thread_id
            and isinstance(row.get("pane_id"), int)
            and row["pane_id"] > 0
        ] if panes.returncode == 0 and isinstance(rows, list) else []
        if len(matches) == 1:
            return session, matches[0]["pane_id"]
        time.sleep(0.5)
    raise RetaskTransportError("thread_view_join_missed")


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
        from fno.dispatch_flags import resolve_dispatch_harness

        harness = resolve_dispatch_harness(None, env=env)[0]
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
        substrate=_flag_value(resolved, "--substrate"),
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
    substrate = entry.substrate
    if substrate not in {"pane", "thread"}:
        return {"outcome": "refused", "reason": "worker_substrate_unknown"}
    mux = entry.mux if isinstance(entry.mux, dict) else None
    thread_id = entry.fno_id
    if substrate == "pane" and (not mux or not mux.get("session") or not mux.get("pane_id")):
        return {"outcome": "refused", "reason": "worker_has_no_mux_ref"}
    if substrate == "thread" and (
        mux is not None or not isinstance(thread_id, str) or not thread_id.strip()
    ):
        return {"outcome": "refused", "reason": "worker_has_no_thread_ref"}
    mux_ref = (
        {"session": mux["session"], "pane_id": mux["pane_id"]} if mux else None
    )
    if not entry.harness_session_id:
        return {"outcome": "refused", "reason": "worker_has_no_session_id"}

    if target.permission_mode is not None:
        return {"outcome": "spawn_required", "reason": "permission_mode"}
    if target.account is not None:
        return {"outcome": "spawn_required", "reason": "account"}
    current_axes = {
        "harness": entry.harness,
        "provider": entry.provider,
        "substrate": substrate,
    }
    target_axes = {
        "harness": target.harness,
        "provider": target.provider if target.provider is not None else entry.provider,
        "substrate": target.substrate if target.substrate is not None else substrate,
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
        "mux": mux_ref,
        "thread_id": entry.fno_id,
        "node": node,
        "target": {**asdict(target), "substrate": target_axes["substrate"]},
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
    # Exact (case-insensitive) match first: bare substring containment picks
    # "gpt-5.6-sol-mini" when the target is "gpt-5.6-sol". Fallback to the
    # most specific containing label (shortest wins), so an alias like "sol"
    # still lands on "gpt-5.6-sol" over "gpt-5.6-sol-mini".
    key = target.strip().lower()
    containing = [(row, label) for row, _cursor, label in rows if key and key in label.lower()]
    exact = [row for row, label in containing if label.strip().lower() == key]
    if len(exact) == 1:
        desired = exact[0]
    elif containing:
        desired = min(containing, key=lambda pair: len(pair[1]))[0]
    else:
        desired = None
    if current is None or desired is None:
        return None
    return desired - current


def _walk_menu(
    frame: str,
    *,
    target: str,
    send: Callable[[str, bool], bool],
    read_frame: Callable[[], str],
    settle: Callable[[], None] = lambda: None,
) -> bool:
    delta = _menu_delta(frame, target)
    if delta is None:
        return False
    arrow = "\x1b[B" if delta > 0 else "\x1b[A"
    for _ in range(abs(delta)):
        if not send(arrow, False):
            return False
    settle()
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
    restamp: Callable[[], object],
    rename: Callable[[str], Optional[str]],
    project_tier: Callable[[str, str], None] = lambda _model, _effort: None,
    ready_frame: Optional[Callable[[str], Mapping[str, object]]] = None,
    settle: Callable[[], None] = lambda: None,
    source_preflight: Optional[Callable[[AgentEntry], Mapping[str, object]]] = None,
) -> dict:
    """Run the bounded retask transaction through injected pane seams."""

    def settled_read() -> str:
        settle()
        return read_frame()

    refusal = {
        "status": "refused",
        "cleared": False,
        "session_restamped": False,
        "switch": "not_started",
        "switch_verified": False,
        "target_submit_confirmed": False,
    }
    strategy = capabilities(entry.harness)["model_switch_strategy"]
    desired_model = target.model or entry.model
    desired_effort = target.effort or entry.effort
    if strategy["kind"] == "unsupported":
        return {**refusal, "reason": "unsupported_switch_strategy"}
    if source_preflight is not None:
        source = source_preflight(entry)
        if source.get("status") != "ready":
            return {**refusal, **source}
    planned = detect_retask(entry, target, node=node)
    if planned["outcome"] in {"spawn_required", "refused"}:
        return {**refusal, "reason": planned.get("reason", planned["outcome"])}
    initial_frame = read_frame()
    if not initial_frame.strip():
        return {**refusal, "reason": "pane_frame_unreadable"}
    verdict = ready_frame(initial_frame) if ready_frame is not None else {}
    if not isinstance(verdict, Mapping) or not (
        verdict.get("matched") and verdict.get("rule_id") and verdict.get("state")
    ):
        return {**refusal, "reason": "pane_state_unobserved"}
    ready_marker = capabilities(entry.harness)["ready_marker"]
    if verdict.get("state") != "idle" or verdict.get("rule_id") != ready_marker:
        return {**refusal, "reason": "pane_not_idle"}
    if not send("/clear", True):
        return {**refusal, "reason": "clear_not_confirmed"}
    predecessor = entry.harness_session_id or ""
    transition = _transition_receipt(restamp(), predecessor)
    if transition is None:
        return {**refusal, "cleared": True, "reason": "session_transition_unconfirmed"}
    if transition.get("classification") != "succession":
        return {
            **refusal,
            "cleared": True,
            "reason": transition.get("reason") or "session_transition_not_succession",
        }
    if transition.get("predecessor_session_id") != predecessor:
        return {**refusal, "cleared": True, "reason": "clear_predecessor_mismatch"}
    new_session = transition.get("current_session_id")
    if not isinstance(new_session, str) or not new_session or new_session == predecessor:
        return {**refusal, "cleared": True, "reason": "session_transition_unconfirmed"}
    if transition.get("registry_rows") != 1:
        return {**refusal, "cleared": True, "reason": "successor_row_count_invalid"}
    if transition.get("lineage_recorded") is not True:
        return {**refusal, "cleared": True, "reason": "successor_lineage_unrecorded"}
    renamed = rename(f"target-{node}")
    if not renamed:
        return {
            **refusal,
            "cleared": True,
            "session_restamped": True,
            "reason": "registry_rename_refused",
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
    cleared_tier = _status_tier(entry.harness, settled_read())
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
    desired_model = target.model or cleared_tier["model"]
    desired_effort = target.effort or cleared_tier["effort"]
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
            settled_read(), target=desired_model or "", send=send,
            read_frame=read_frame, settle=settle,
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
            settled_read(), target=effort_label, send=send,
            read_frame=read_frame, settle=settle,
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
        verified = _status_tier(entry.harness, settled_read())
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
        "source_session_id": predecessor,
        "current_session_id": new_session,
        "transition": "succession",
        "registry_rows": 1,
        "lineage_recorded": True,
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
    node = _resolve_retask_node(node)
    entry = resolve_agent(worker, path=registry_path).entry
    target = resolve_target_coordinate(
        node,
        settings=settings,
        model=model,
        effort=effort,
        env=env,
    )
    if entry.substrate == "thread":
        session, pane_id = resolve_thread_viewport(entry)
        pane = str(pane_id)
    else:
        mux = entry.mux or {}
        session = str(mux.get("session"))
        pane = str(mux.get("pane_id"))
    renamed_name = [entry.name]
    restamped_session = [entry.harness_session_id]
    clear_sent = [False]

    def read_frame() -> str:
        try:
            result = subprocess.run(
                ["fno", "mux", "pane", "read", "--session", session, pane, "--lines", "80"],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise RetaskTransportError("pane_read_timeout") from exc
        return result.stdout if result.returncode == 0 else ""

    def settle() -> None:
        # Wait for the TUI redraw before the next read.
        try:
            subprocess.run(
                [
                    "fno", "mux", "pane", "wait", "--session", session, pane,
                    "--quiet-ms", "400", "--timeout", "8",
                ],
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise RetaskTransportError("pane_wait_timeout") from exc

    def settled_read() -> str:
        settle()
        return read_frame()

    def send(text: str, submit: bool) -> bool:
        command = [
            "fno", "mux", "pane", "send", "--session", session, pane,
            "--text", text, "--raw",
        ]
        if submit:
            command.append("--submit")
        try:
            result = subprocess.run(command, capture_output=True, text=True, timeout=15, check=False)
        except subprocess.TimeoutExpired as exc:
            raise RetaskTransportError("pane_send_timeout") from exc
        if text == "/clear" and submit and result.returncode == 0:
            clear_sent[0] = True
        return result.returncode == 0

    def restamp() -> object:
        clear_frame = settled_read()
        predecessor = entry.harness_session_id or ""
        match = re.search(
            r"To continue this session, run codex resume (?P<predecessor>[^\s]+)",
            clear_frame,
        )
        if match and match.group("predecessor") != predecessor:
            return {
                "classification": "deferred",
                "reason": "clear_predecessor_mismatch",
                "predecessor_session_id": match.group("predecessor"),
            }
        # Use the shared transition classifier for lineage decisions.
        for _ in range(40):
            rows = load_registry(path=registry_path)
            branch = next(
                (
                    candidate
                    for candidate in rows
                    if candidate.harness == entry.harness
                    and candidate.forked_from_session_id == predecessor
                    and candidate.harness_session_id
                    and classify_session_transition(
                        predecessor, candidate.harness_session_id, True
                    )
                    == "branch"
                ),
                None,
            )
            if branch is not None:
                return {
                    "classification": "branch",
                    "reason": "session_transition_not_succession",
                    "predecessor_session_id": predecessor,
                    "current_session_id": branch.harness_session_id,
                }
            for candidate in rows:
                if candidate.name == entry.name and candidate.harness_session_id:
                    if candidate.harness_session_id != predecessor:
                        if not match:
                            return {
                                "classification": "deferred",
                                "reason": "clear_predecessor_unconfirmed",
                                "predecessor_session_id": predecessor,
                                "current_session_id": candidate.harness_session_id,
                            }
                        restamped_session[0] = candidate.harness_session_id
                        if predecessor not in (candidate.predecessor_session_ids or []):
                            return {
                                "classification": "deferred",
                                "reason": "successor_lineage_unrecorded",
                                "predecessor_session_id": predecessor,
                                "current_session_id": candidate.harness_session_id,
                            }
                        if classify_session_transition(
                            predecessor, candidate.harness_session_id, False
                        ) != "succession":
                            return {
                                "classification": "deferred",
                                "reason": "session_transition_not_succession",
                                "predecessor_session_id": predecessor,
                                "current_session_id": candidate.harness_session_id,
                            }
                        return {
                            "classification": "succession",
                            "predecessor_session_id": predecessor,
                            "current_session_id": candidate.harness_session_id,
                            "registry_rows": sum(
                                1
                                for row in load_registry(path=registry_path)
                                if row.harness == entry.harness
                                and row.harness_session_id == candidate.harness_session_id
                            ),
                            "lineage_recorded": True,
                        }
            time.sleep(0.25)
        return None

    def rename(new_name: str) -> Optional[str]:
        try:
            renamed_name[0] = rename_agent(
                entry.name, new_name, node=node, registry_path=registry_path
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

    def ready_frame(frame: str) -> Mapping[str, object]:
        from fno.agents.mux_spawn import _evaluate_manifest_screen, _pane_osc_title

        osc_title = _pane_osc_title(session, int(pane), subprocess.run)
        if entry.harness == "claude" and osc_title is None:
            return {"matched": False, "error": "pane title unreadable"}
        return _evaluate_manifest_screen(
            entry.harness, frame, subprocess.run, osc_title=osc_title
        )

    try:
        return execute_retask(
            entry,
            target,
            node=node,
            read_frame=read_frame,
            send=send,
            restamp=restamp,
            rename=rename,
            project_tier=project_tier,
            ready_frame=ready_frame,
            settle=settle,
            source_preflight=_source_preflight,
        )
    except RetaskTransportError as exc:
        # Preserve the partial transaction state in the refusal receipt.
        restamped = restamped_session[0] != entry.harness_session_id
        receipt = {
            "status": "refused",
            "cleared": clear_sent[0] or restamped,
            "session_restamped": restamped,
            "switch": "not_started",
            "switch_verified": False,
            "target_submit_confirmed": False,
            "reason": str(exc),
        }
        if renamed_name[0] != entry.name:
            receipt["registry_name"] = renamed_name[0]
        return receipt


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
    node = _resolve_retask_node(node)
    entry = resolve_agent(worker, path=registry_path).entry
    target = resolve_target_coordinate(
        node,
        settings=settings,
        model=model,
        effort=effort,
        env=env,
    )
    return detect_retask(entry, target, node=node)
