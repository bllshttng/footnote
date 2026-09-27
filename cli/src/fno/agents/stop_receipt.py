"""The stop verb verifies its own receipt (task 3.1).

Measured: `claude stop` exited 0 over a row that never left the wake set,
`stopped:` printed beside it; the refusal carries the citizen-vs-teammate
split (an Agent-tool teammate needs `fno agents rm`).
"""
from __future__ import annotations

from typing import Optional

from fno.agents import events
from fno.agents.registry import AgentEntry, load_registry


def emit_shellout_stop(*, name: str, claude_exit: Optional[int], short_id: str) -> None:
    """The one `agent_stopped` row for a shellout stop, keyed by what ran."""
    events.emit(
        "agent_stopped",
        name=name,
        provider="claude",
        claude_exit=claude_exit,
        short_id=short_id,
        stopped_by="shellout",
    )


def wake_set_refusal(name: str, short_id: str, existing: AgentEntry) -> Optional[str]:
    """Refusal text when the stopped row still reads live or the recorded
    process provably survived the ask, else None.

    Compared against the session that WAS stopped: a successor that took the
    row over during the shellout (a restamp, a duplicate) is not a stop
    failure, and an unreadable registry answers nothing.
    """
    from fno.agents.registry import _OWNERSHIP_LIVE_STATUSES

    try:
        reread = next(
            (
                e
                for e in load_registry()
                if e.harness == "claude" and (e.name == name or name in e.aliases)
            ),
            None,
        )
    except Exception:  # noqa: BLE001 - an unreadable registry answers nothing
        return None
    if (
        reread is not None
        and reread.status in _OWNERSHIP_LIVE_STATUSES
        and (reread.harness_session_id, reread.short_id or "")
        == (existing.harness_session_id, short_id or "")
    ):
        return (
            f"claude stop reported success but {name} ({short_id}) still reads live "
            f"in the registry. Citizen and teammate rows tear down differently: "
            f"an fno-spawned citizen pane/thread stops through this verb, an "
            f"Agent-tool teammate needs `fno agents rm {name}`."
        )
    if reread is None or (
        reread.harness_session_id, reread.short_id or ""
    ) != (existing.harness_session_id, short_id or ""):
        return None
    # The row reads terminal because the stop itself just wrote it, so the
    # receipt must prove the PROCESS died: a sweep honestly re-marks a
    # surviving process's row. The token gate blocks a recycled-pid false positive.
    from fno.agents.spawn_gate import _pid_alive

    if existing.pid_start_time is None:
        return None
    if _pid_alive(existing.pid, existing.pid_start_time) is True:
        return (
            f"claude stop reported success but pid {existing.pid} for {name} "
            f"is still alive; the row would be re-marked live and counted "
            f"against the spawn share again. Remedy: `fno agents rm {name}` "
            f"ends the process and clears the row."
        )
    return None
