"""fno MCP channel surface (Phase 5 — US6).

Two artifacts ship in this package, with two different lifecycle owners
(per spec Locked Decision 16):

- ``channel_server`` — the per-session stdio MCP server. Claude Code
  spawns it as a subprocess via the fno plugin's ``.mcp.json``.
  Declares the ``claude/channel`` experimental capability. Forwards
  externally-originated pokes (received from the sidecar) into the
  Claude session as ``notifications/claude/channel``. Lifecycle owned
  by CC.
- ``sidecar`` — the per-user Unix-socket daemon at
  ``~/.fno/sidecar.sock`` (or ``$XDG_RUNTIME_DIR/fno/...``).
  Lazy-start / lazy-exit. Holds the in-memory ``session_id -> channel
  server stdio pipe`` map. Lifecycle owned by fno.

``channel`` is the shared wire-format module (envelope build /
validate). ``client`` is the thin client fno CLI processes use to talk
to the sidecar.
"""
from __future__ import annotations

from fno.mcp.channel import (
    ENVELOPE_VERSION,
    META_KEY_RE,
    MCP_CHANNEL_METHOD,
    MCPChannelEnvelopeError,
    build_channel_notification,
    validate_envelope,
)

__all__ = [
    "ENVELOPE_VERSION",
    "META_KEY_RE",
    "MCP_CHANNEL_METHOD",
    "MCPChannelEnvelopeError",
    "build_channel_notification",
    "validate_envelope",
]
