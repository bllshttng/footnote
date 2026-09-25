"""Read-only planning for reusing one mux worker on its existing node."""
from __future__ import annotations

import json
import io
import os
import subprocess
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Mapping, Optional, Sequence

from fno.agents.harness_map import (
    DispatchResolveError, dispatch_command,
    normalize_command,
)
from fno.agents.mux_spawn import resolve_mux_session
from fno.agents.naming import parse_many
from fno.agents.registry import (
    AgentEntry,
    resolve_agent,
)
from fno.agents.spawn_defaults import inject_spawn_defaults


class RetaskTransportError(RuntimeError):
    """A pane read or send exceeded its bounded transport timeout."""

    def __init__(self, reason: str, detail: Optional[str] = None):
        super().__init__(reason)
        self.detail = detail


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
    # The node's next lifecycle verb; profiles.<verb> supplies the tier.
    verb: str = "target"


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
    session = resolve_mux_session(None).strip()
    if not isinstance(thread_id, str) or not thread_id.strip():
        # Name the row defect. A bare transport code here read as a broken
        # pipe, so an absent field looked like something a retry could fix.
        # Same reason word `detect_retask` already uses for this case.
        raise RetaskTransportError(
            f"worker_has_no_thread_ref: {entry.name} is "
            f"substrate={entry.substrate or 'unknown'} and its registry row "
            "carries no thread reference, so there is no thread to open. "
            "A retry cannot fix it."
        )
    fno_bin = os.environ.get("FNO_BIN") or "fno"

    def invoke(args: list[str], timeout: int) -> subprocess.CompletedProcess[str]:
        try:
            return runner([fno_bin, *args], capture_output=True, text=True, timeout=timeout, check=False)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise RetaskTransportError("thread_view_open_timeout") from exc

    # The door answers the row NAME, not the session uuid; fno_id guards the
    # join. A machine reach asks for a portal of its own in a new tab: with
    # no --portal the server serves portal 0, and the reach would repoint the
    # operator's own seat and leave the view under their keys.
    door = invoke(
        ["mux", "thread", "--server", session, entry.name, "--portal", "new", "--tab", "new"],
        30,
    )
    if door.returncode:
        lines = (door.stderr or door.stdout or "").strip().splitlines()
        raise RetaskTransportError(
            "thread_view_unavailable",
            detail=lines[-1] if lines else f"exit {door.returncode}",
        )
    # The pane opened above stays open on a join miss and its name stamping
    # can lag the open, so the join retries; the miss names the opened pane.
    for _ in range(3):
        try:
            panes = invoke(["mux", "pane", "ls", "--server", session, "--json"], 10)
            rows = json.loads(panes.stdout)
        except RetaskTransportError:
            raise
        except ValueError as exc:
            # A different cause from the absent field above: the pane listing
            # itself came back unparseable. One word for both is what made the
            # row defect read as a transport failure.
            raise RetaskTransportError("thread_pane_listing_unreadable") from exc
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
    # The node's next lifecycle verb; an abstain (None) means ``target``.
    # The table answers canonical "/blueprint"; probe and rename take the
    # bare word. One lookup and one wrapper, shared with the door.
    from fno.agents.node_dispatch import find_node_row, node_effective_verb

    verb = (node_effective_verb(find_node_row(node), node_id=node) or "target").lstrip(
        "/"
    ) or "target"
    args = ["spawn", "--name", "retask-probe"]
    if model is not None:
        args += ["--model", model]
    if effort is not None:
        args += ["--effort", effort]
    args.append(f"/fno:{verb} {node}")
    # a probe, not a real dispatch - the builtin rung would otherwise
    # read as an explicit override and force every retask to respawn.
    resolved = inject_spawn_defaults(
        args,
        settings=settings,
        env=env,
        stderr=io.StringIO(),
        apply_permission_builtin=False,
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
        verb=verb,
    )


def finished_planner(
    entries: Sequence[AgentEntry],
    *,
    node_id: str,
    graph: Mapping[str, dict],
    project_id: str,
    project_of: Callable[[str], str],
) -> Optional[AgentEntry]:
    """The earliest-finished live blueprint worker on the same epic, else None."""
    parent = (graph.get(node_id) or {}).get("parent")
    if not parent:
        return None
    candidates: list[tuple[str, AgentEntry]] = []
    for entry, parsed in zip(entries, parse_many([e.name for e in entries])):
        if entry.status != "live" or entry.substrate not in {"pane", "thread"}:
            continue
        if parsed is None or parsed.verb != "bp":
            continue
        inside = entry.inside_leg or {}
        if inside.get("state") != "done":
            continue
        if project_of(entry.cwd) != project_id:
            continue
        row_node = entry.node or (parsed.node if parsed is not None else None)
        if not row_node or row_node == node_id:
            continue
        row_rec = graph.get(row_node) or {}
        if row_rec.get("parent") != parent:
            continue
        sessions = [s for s in row_rec.get("sessions") or [] if isinstance(s, dict)]
        if not any(s.get("phase") == "blueprint" and s.get("ended_at") for s in sessions):
            continue
        candidates.append((inside.get("received_at") or "", entry))
    return min(candidates, key=lambda pair: pair[0])[1] if candidates else None


def _refused(reason: str, **overrides: object) -> dict:
    """The shared refusal receipt; overrides restate the true partial state."""
    return {
        "status": "refused",
        "cleared": False,
        "session_restamped": False,
        "switch": "not_started",
        "switch_verified": False,
        "target_submit_confirmed": False,
        "reason": reason,
        **overrides,
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
    """Resolve the retask coordinate and hand the transaction to fno-agents."""
    node = _resolve_retask_node(node)
    entry = resolve_agent(worker, path=registry_path).entry
    try:
        target = resolve_target_coordinate(
            node,
            settings=settings,
            model=model,
            effort=effort,
            env=env,
        )
    except DispatchResolveError as exc:
        return _refused("dispatch_verb_unresolved", detail=str(exc))
    try:
        if entry.substrate == "thread":
            resolved_session, resolved_pane_id = resolve_thread_viewport(entry)
            mux = {"session": resolved_session, "pane_id": resolved_pane_id}
        else:
            mux = entry.mux or {}
        if target.verb == "target":
            command_template = dispatch_command(target.harness)
        else:
            command_template = normalize_command(f"/{target.verb} {{id}}", target.harness)
        from fno.rust_binary import verb_call

        return verb_call(
            "rename",
            {
                "op": "retask",
                "worker": entry.name,
                "node": node,
                "target": asdict(target),
                "target_command": command_template.format(id=node),
                "mux": mux,
            },
            RetaskTransportError,
            timeout=300,
        )
    except RetaskTransportError as exc:
        receipt = _refused(str(exc))
        if exc.detail:
            receipt["detail"] = exc.detail
        return receipt
