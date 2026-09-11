"""The stop verb verifies its own receipt (x-dead task 3.1).

A verb that reports success while the thing is still counted as live is its
own defect: measured 2026-09-11, `claude stop` exited 0 over an Agent-tool
teammate row, the row never left the wake set, and `stopped: <name>` printed
beside it. The receipt re-reads the row and refuses the lie. The refusal is
also where the citizen-vs-teammate fact lives: an fno-spawned citizen
pane/thread stops through `claude stop`; an Agent-tool teammate needs
`fno agents rm`.
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


def wake_set_refusal(
    name: str, short_id: str, existing: AgentEntry
) -> Optional[str]:
    """Refusal text when the stopped row still reads live, else None.

    The comparison is against the session that WAS stopped: a successor that
    took the row over during the shellout (a restamp, a duplicate) is not a
    failure of the stop, and a registry read failure must not turn a
    successful stop into a refusal.
    """
    from fno.agents.registry import _OWNERSHIP_LIVE_STATUSES

    try:
        reread = next(
            (
                entry
                for entry in load_registry()
                if entry.harness == "claude"
                and (entry.name == name or name in entry.aliases)
            ),
            None,
        )
    except Exception:  # noqa: BLE001 - an unreadable registry answers nothing
        return None
    if (
        reread is None
        or getattr(reread, "status", "") not in _OWNERSHIP_LIVE_STATUSES
        or getattr(reread, "harness_session_id", "")
        != getattr(existing, "harness_session_id", "")
        or (getattr(reread, "short_id", "") or "") != (short_id or "")
    ):
        return None
    return (
        f"claude stop reported success but {name} ({short_id}) still reads live "
        f"in the registry. Citizen and teammate rows tear down differently: "
        f"an fno-spawned citizen pane/thread stops through this verb, an "
        f"Agent-tool teammate needs `fno agents rm {name}`."
    )
