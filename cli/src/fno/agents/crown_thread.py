"""How a pre-planned spawn crown lands on a codex thread row.

The Rust app-server lane mints the row, so unlike the pane and claude-bg
doors (which stamp rows they mint themselves) this settlement stamps an
EXISTING row: plan before launch - fail closed, so a refusal launches
nothing - then stamp, arm the king manifest, and journal after.
"""

from __future__ import annotations

import sys
from dataclasses import replace
from pathlib import Path
from typing import Optional

from fno.agents.crown import (
    calling_agent_row,
    journal_spawn_crown,
    plan_spawn_crown,
    settle_spawn_crown,
)
from fno.agents.registry import RegistryVersionError, update_registry


def reign_typed_message(
    message: str,
    crown_level: Optional[int],
    crown_scope: Optional[str],
    revive: bool,
) -> tuple[str, bool]:
    """A crowned spawn's payload opens with the plugin-qualified reign verb.

    A revival keeps its own payload (the session already knows what it is);
    the receipt names the not-typed case with the remedy.
    """
    if crown_level is not None and crown_scope and not revive:
        return f"/fno:lead {crown_scope}\n{message}", True
    return message, False


def plan_codex_thread_crown(
    message: str,
    crown_level: int,
    crown_scope: Optional[str],
    succession: bool,
    harness: str,
) -> dict:
    """Decide the crown BEFORE the thread spawns and type the reign verb.

    Same fail-closed plan the claude bg branch runs inside its flock:
    authority and occupancy are decided before anything is created, so a
    refusal launches nothing and leaves the scope dispatchable. Raises
    :class:`DispatchAskError` on a refused plan; returns the typed message,
    the plan, and the caller name the settle and receipts need.
    """
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.harness_map import normalize_command

    caller_row = calling_agent_row()
    crown_caller_name = getattr(caller_row, "name", None)
    crown_refusal, crown_plan = plan_spawn_crown(crown_scope or "", caller_row, succession)
    if crown_refusal is not None:
        raise DispatchAskError(f"--crown: {crown_refusal}", exit_code=2)
    message, reign_typed = reign_typed_message(message, crown_level, crown_scope, revive=False)
    if reign_typed:
        # The king's first turn is the reign verb itself, normalized to the
        # receiving harness's verb spelling ($fno:lead for codex).
        message = normalize_command(message, harness)
    return {
        "message": message,
        "reign_typed": reign_typed,
        "crown_plan": crown_plan,
        "crown_caller_name": crown_caller_name,
    }


def settle_codex_thread_crown(
    ask: dict,
    *,
    name: str,
    cwd: Path,
    session_id: str,
    crown_level: int,
    crown_scope: Optional[str],
    parent_edge: Optional[tuple],
) -> None:
    """Carry the settled plan onto the codex thread row, after launch.

    The lane enqueues the seed before its receipt returns, so the stamp lands
    just after; a row exists uncrowned briefly either way - the crown verb's
    own shape.
    """
    crown_grantor_val = (parent_edge or _capture_parent_edge_now())[0] or "human"
    crown_outcome: Optional[str] = None
    crown_cleared: list = []
    king_loop_armed: Optional[bool] = None
    king_unarmed_reason = ""
    heir_found = False

    def _stamp_heir(entries: list) -> list:
        nonlocal crown_outcome, crown_cleared, king_loop_armed, king_unarmed_reason
        nonlocal heir_found
        assert ask["crown_plan"] is not None  # set by the pre-launch plan call
        entries, crown_outcome, crown_cleared = settle_spawn_crown(
            entries, scope=crown_scope or "", plan=ask["crown_plan"],
            heir=name, heir_harness="codex", heir_session=session_id,
            heir_cwd=str(cwd),
        )
        level, scope_v, grantor_v = (
            (None, None, None)
            if crown_outcome == "declined"
            else (crown_level, crown_scope, crown_grantor_val)
        )
        entries = [
            replace(e, crown_level=level, crown_scope=scope_v, crown_grantor=grantor_v)
            if e.name == name else e
            for e in entries
        ]
        heir = next((e for e in entries if e.name == name), None)
        heir_found = heir is not None and crown_outcome != "declined"
        if heir is not None and heir.crown_level is not None and heir.crown_scope:
            from fno.king.state import arm_king_manifest

            try:
                king_loop_armed = arm_king_manifest(
                    heir.crown_scope, heir.harness_session_id or "", row=heir,
                ) is not None
            except ValueError as exc:
                king_loop_armed = False
                king_unarmed_reason = str(exc)
        return entries

    try:
        update_registry(_stamp_heir)
        journal_spawn_crown(
            crown_outcome, crown_cleared,
            name=name, level=crown_level, scope=crown_scope, grantor=crown_grantor_val,
        )
        reign_typed = ask["reign_typed"]
        if crown_outcome == "declined":
            print(
                f"spawn: crown declined (scope {crown_scope!r} already held by a "
                "live row); the worker launched without a crown.",
                file=sys.stderr,
            )
        elif crown_outcome == "succeeded":
            vacated = sorted({row.name for row, cause in crown_cleared if cause == "succession"})
            noted = ask["crown_caller_name"] in vacated
            print(
                f"spawn: crown over {crown_scope!r} transferred from {', '.join(vacated)} "
                f"to {name} (succession)." + (" You no longer hold it." if noted else ""),
                file=sys.stderr,
            )
        if crown_scope and crown_outcome != "declined" and not heir_found:
            print(
                f"spawn: crown over {crown_scope!r} NOT applied: no registry row "
                f"named {name!r}; grant it with `fno agents crown`",
                file=sys.stderr,
            )
        elif crown_scope and crown_outcome != "declined" and king_loop_armed is False:
            why = f": {king_unarmed_reason}" if king_unarmed_reason else "; king loop disabled"
            print(
                f"spawn: crown over {crown_scope!r} recorded, but the king loop "
                f"manifest was NOT armed{why}",
                file=sys.stderr,
            )
        if crown_scope and heir_found:
            print(
                f"spawn: crown over {crown_scope!r} recorded; "
                + ("reign typed" if reign_typed else "reign NOT typed"),
                file=sys.stderr,
            )
    except (OSError, ValueError, RegistryVersionError) as exc:
        # The worker is ALIVE (the Rust lane supervises it); only the crown
        # failed to land, so the spawn still succeeds.
        print(
            f"spawn: crown settlement failed after launch: {exc}; {name!r} runs "
            "UNCROWNED. Grant it with `fno agents crown` once it self-identifies.",
            file=sys.stderr,
        )


def _capture_parent_edge_now() -> tuple:
    from fno.agents.dispatch import _capture_parent_edge

    return _capture_parent_edge()
