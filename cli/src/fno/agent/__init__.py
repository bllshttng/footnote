"""fno self-introspection: read-only operating-context surface.

Two commands (``fno whoami`` / ``fno status``) give an agent operating fno a
curated view of its operating context (fleet -> walker -> session). Read-only;
no state mutations, no events emitted. Formerly the ``fno agent`` (singular)
namespace; retired in ab-12dd2a5d (the ``suggest`` / ``capabilities`` verbs were
trimmed and the survivors relocated to top-level). ``fno agents`` (plural, the
dispatch mesh) is unrelated and untouched.

See internal/fno/plans/2026-05-11-fno-agent-introspection.md.
"""
from fno.agent.cli import status_command, whoami_command
from fno.agent.state import (
    AgentContext,
    AgentOptions,
    FleetState,
    MalformedStateError,
    MissingStateFileOverrideError,
    SessionState,
    WalkerState,
    load_agent_context,
)

__all__ = [
    "AgentContext",
    "AgentOptions",
    "FleetState",
    "MalformedStateError",
    "MissingStateFileOverrideError",
    "SessionState",
    "WalkerState",
    "load_agent_context",
    "status_command",
    "whoami_command",
]
