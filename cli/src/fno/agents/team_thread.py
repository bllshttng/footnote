"""Transport for native thread promotion; shared writers own persisted effects."""
from dataclasses import asdict
from pathlib import Path
import sys
from typing import Any, Optional

from fno.agents.crown import calling_agent_row, journal_spawn_crown, plan_spawn_crown, settle_spawn_crown
from fno.agents.registry import AgentEntry, RegistryVersionError, update_registry
from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call


def lead_typed_message(message: str, level: Optional[int], scope: Optional[str],
                       revive: bool, harness: str = "claude") -> tuple[str, bool]:
    answer = spawn_overlay_call({"kind": "spawn-team", "op": "seed", "message": message,
                                 "level": level, "scope": scope, "revive": revive, "harness": harness})
    return answer["message"], answer["typed"]


def plan_thread_promotion(message: str, level: int, scope: Optional[str], succession: bool,
                          harness: str, parent_edge: Optional[tuple], *, revive: bool = False) -> dict:
    from fno.agents.dispatch import DispatchAskError, _capture_parent_edge

    caller = calling_agent_row()
    refusal, plan = plan_spawn_crown(scope or "", caller, succession)
    if refusal is not None:
        raise DispatchAskError(f"--promote: {refusal}", exit_code=2)
    try:
        message, typed = lead_typed_message(message, level, scope, revive, harness)
    except SpawnOverlayUnavailable as exc:
        raise DispatchAskError(f"--promote: {exc}", exit_code=2) from exc
    return {"message": message, "typed": typed, "plan": plan, "caller": getattr(caller, "name", None),
            "level": level, "scope": scope, "grantor": (parent_edge or _capture_parent_edge())[0] or "human",
            "harness": harness}


def settle_thread_promotion(ask: dict, *, name: str, cwd: Path, session_id: str) -> None:
    from fno.king.state import arm_king_manifest

    answer: dict[str, Any] = {**ask, "name": name, "found": False, "armed": None, "arm_error": ""}
    vacated: list = []
    def stamp(rows: list[AgentEntry]) -> list[AgentEntry]:
        nonlocal vacated
        rows, answer["outcome"], vacated = settle_spawn_crown(
            rows, scope=ask["scope"], plan=ask["plan"], heir=name, heir_harness=ask["harness"],
            heir_session=session_id, heir_cwd=str(cwd), stamp=True,
            level=ask["level"], grantor=ask["grantor"])
        heir = next((r for r in rows if r.name == name and
                     (r.harness_session_id or r.cc_session_id or r.short_id) == session_id), None)
        answer["found"] = heir is not None
        if heir is not None and heir.crown_scope and answer["outcome"] != "declined":
            try:
                answer["armed"] = arm_king_manifest(heir.crown_scope, session_id, row=heir) is not None
            except ValueError as exc:
                answer["armed"], answer["arm_error"] = False, str(exc)
        return rows
    try:
        update_registry(stamp)
        journal_spawn_crown(answer["outcome"], vacated, name=name, level=ask["level"],
                            scope=ask["scope"], grantor=ask["grantor"])
        answer["vacated"] = [(asdict(row), cause) for row, cause in vacated]
        for note in spawn_overlay_call({**answer, "kind": "spawn-team", "op": "receipt"})["notices"]:
            print(note, file=sys.stderr)
    except (OSError, ValueError, RegistryVersionError, SpawnOverlayUnavailable) as exc:
        print(f"spawn: promotion settlement failed after launch: {exc}; {name!r} runs "
              "without a confirmed role. Grant it with fno agents org promote.", file=sys.stderr)
