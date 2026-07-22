# fno agents — MCP channel + sidecar

The MCP channel backend gives `fno agents ask` a second send path that rides on Anthropic's sanctioned `claude/channel` capability instead of the reverse-engineered `messagingSocketPath` Unix-domain socket. Both backends are supported indefinitely; the dispatcher prefers MCP when available and falls back to the socket on probe failure. The user-visible reply is identical regardless of which backend delivered the message — only the wire transport differs.

Parent: [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md). The socket path it complements: [fno-agents-followup.md](fno-agents-followup.md).

## Two artifacts, two lifecycle owners

```
+--------------------+        +-----------------+        +--------------+
|   fno agents ask   |        | footnote side- |        |  channel_    |
|     (CLI caller)   |--Unix->|  car daemon     |--Unix->|  server      |
|                    |  sock  |  (per-user)     |  push  |  (per-       |
|                    |        |                 | -down  |   session,   |
|                    |        |                 |  conn  |   CC-owned)  |
+--------------------+        +-----------------+        +--------------+
                                                                |
                                                                | stdio
                                                                v
                                                       +---------------+
                                                       | Claude Code   |
                                                       | session       |
                                                       +---------------+
```

Both the per-user sidecar daemon and the per-session channel-server child ship in `cli/src/fno/mcp/`, but they have **different lifecycle owners**:

- **Sidecar (`sidecar.py`)** is fno-owned. Lazy-start / lazy-exit. The bind path resolves to `$XDG_RUNTIME_DIR/fno/sidecar.sock` if set, else `paths.state_dir() / "sidecar.sock"`. Single-leader via socket-bind exclusivity. Idle-exits after 30 minutes with zero registered channels.
- **Channel server (`channel_server.py`)** is CC-owned. CC reads the footnote plugin's `.mcp.json` at session start and spawns the child as a stdio subprocess. fno has no `start`/`stop` for it.

The sidecar exists because CC spawns one channel server per session and they don't share pipes. Without a per-user rendezvous, external `fno agents ask` pokes from sibling processes cannot reach the right channel server. The sidecar holds an in-memory `session_id -> writer` map and routes pokes to the registered child.

## Routing decision tree

For a follow-up against an existing claude agent (the read-path lives in `dispatch.py:_followup`):

```
1. Read AgentEntry under with_agent_lock_and_entry.
2. If entry.provider != "claude" -> existing socket path (no MCP).
3. If entry.mcp_channel_id is None -> socket path.
4. Else:
   a. Probe via mcp_channel_reachable(mcp_channel_id, timeout=250ms).
   b. True  -> ask_followup_via_mcp(...). emit agent_followup_done(backend="mcp").
   c. False -> ask_followup(...). emit mcp_channel_demoted_to_socket(reason="channel_not_registered").
   d. Raises ReachabilityProbeError ->
        ask_followup(...). emit mcp_channel_unreachable(reason="mcp_channel_disconnected").
   e. probe True but send raises MCPChannelSendError ->
        ask_followup(...). emit mcp_channel_demoted_to_socket(reason="send_failed_post_probe:<...>").
```

The `backend` field on `agent_followup_done` (`"mcp"`, `"socket"`, or `"socket_after_mcp_demote"`) gives forensic analytics a clean way to slice send-path outcomes.

## Modules and their boundaries

```
cli/src/fno/mcp/
├── __init__.py        # exports: ENVELOPE_VERSION, MCP_CHANNEL_METHOD,
│                      # META_KEY_RE, MCPChannelEnvelopeError,
│                      # build_channel_notification, validate_envelope
├── channel.py         # WIRE FORMAT — single source of truth.
│                      # build + validate + drift-diff. Pinned by
│                      # cli/tests/fixtures/mcp_channel_envelope.json
│                      # and self-tested by validate-mcp-channel.sh.
├── channel_server.py  # stdio MCP child. Declares experimental
│                      # claude/channel capability. Forwards inbound
│                      # pokes (received from sidecar) as
│                      # notifications/claude/channel on stdout.
├── sidecar.py         # Unix-socket daemon. Single-leader. Holds
│                      # session_id -> channel-server-writer map.
│                      # stale-socket recovery via
│                      # _prepare_socket_path + retry-once around
│                      # start_unix_server.
└── client.py          # Thin SOCK_STREAM client fno CLI uses to talk
                       # to the sidecar. Lazy-starts the sidecar
                       # daemon if missing.
```

Provider + dispatch surface:

```
cli/src/fno/agents/
├── providers/claude.py    # +MCPChannelSendError, +ask_followup_via_mcp,
│                          #  +mcp_channel_reachable (tri-state).
├── dispatch.py            # +route-selection in _followup helper
│                          # +register_mcp_channel write verb
│                          # +reconcile MCP probe slot
├── registry.py            # AgentEntry gains optional mcp_channel_id field.
└── events.py              # MCP channel KIND_* constants block.
```

## Wire format pin

The `notifications/claude/channel` envelope shape comes from Claude Code's channels reference (§Notification format). We mirror it byte-for-byte:

```json
{
  "jsonrpc": "2.0",
  "method": "notifications/claude/channel",
  "params": {
    "content": "<UTF-8 body>",
    "meta": {"from_name": "...", "session_id": "...", "...": "..."}
  }
}
```

`meta` keys must match `^[A-Za-z0-9_]+$` because CC silently drops non-identifier keys (hyphens, dots, spaces). The builder (`build_channel_notification`) refuses to emit them so the operator sees a loud `MCPChannelEnvelopeError` instead of a silently-stripped attribute.

The smoke script (`cli/scripts/smoke/validate-mcp-channel.sh`, gated behind `MCP_SMOKE=1`) compares the builder's output to the pinned fixture so a CC research-preview API drift FAILS LOUDLY in dev before reaching production callers.

## mcp_channel_id

`mcp_channel_id` is populated from the agent's `claude_short_id` (1:1 mapping) so the sidecar routes by claude's native session id without an id-translation layer. The field type (`Optional[str]`) permits swapping to a server-generated UUIDv4 without a registry-schema bump (a one-line change in `register_mcp_channel`).

## Operational notes

- Sidecar logs land under `$XDG_STATE_HOME/fno/sidecar.log` (default `~/.local/state/fno/sidecar.log`).
- The sidecar state file (`sidecar-state.json`) is flushed on SIGTERM but not on SIGKILL. Channel servers re-register on next session boot; the registry's `mcp_channel_id` is the persistent source of truth.
- macOS AF_UNIX path limit is 104 chars. Long `$HOME` values are rare in practice, but the operator-visible error (`AF_UNIX path too long`) is transparent.
- The smoke script is gated behind `MCP_SMOKE=1` so CI without the footnote CLI venv skips it. Run it manually after bumping the channels-reference doc or upgrading Claude Code.
