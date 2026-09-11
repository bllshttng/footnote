"""The stop verb verifies its own receipt (x-dead task 3.1).

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
    """Refusal text when the stopped row still reads live, else None.

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
        reread is None
        or reread.status not in _OWNERSHIP_LIVE_STATUSES
        or (reread.harness_session_id, reread.short_id or "")
        != (existing.harness_session_id, short_id or "")
    ):
        return None
    return (
        f"claude stop reported success but {name} ({short_id}) still reads live "
        f"in the registry. Citizen and teammate rows tear down differently: "
        f"an fno-spawned citizen pane/thread stops through this verb, an "
        f"Agent-tool teammate needs `fno agents rm {name}`."
    )
