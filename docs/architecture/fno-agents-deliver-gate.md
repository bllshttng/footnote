# Architecture: agent.deliver RPC and the injection gate

This document covers the internals of live PTY delivery for codex and gemini hosted peers: the `agent.deliver` daemon RPC, the `inject_into_pty` primitive, the provenance container format, and the per-provider injection gate that guards them. For the user-facing send verb, see [docs/guides/fno-agents-send.md](../guides/fno-agents-send.md).

## agent.deliver RPC

The Python dispatcher (`cli/src/fno/agents/dispatch.py`, `_deliver_live`) calls the `agent.deliver` JSON-RPC method over the daemon's Unix supervisor socket (`~/.fno/agents/supervisor.sock`). The transport uses 4-byte little-endian u32 length-prefix framing (defined in `crates/fno-agents/src/protocol.rs`).

**Request params:**

```json
{
  "name": "<agent-registry-name>",
  "body": "<message text>",
  "from_name": "<sender identity>"
}
```

**Result shapes** (HTTP 200-equivalent; `"result"` field in the JSON-RPC response):

```json
{"delivered": true, "transport": "pty"}
```

```json
{"delivered": false, "reason": "<reason>"}
```

`reason` values for `delivered: false`:

| reason | Meaning |
|---|---|
| `"claude-routes-via-socket"` | The named agent is a claude peer. Locked Decision 9 prohibits PTY injection for claude; the caller routes via the socket/MCP path instead. |
| `"injection-gate-unverified"` | The provider's injection gate has no passing record. Message goes durable. |
| `"injection-gate-failed"` | The gate has an explicit failed record for this provider. Message goes durable. |
| `"worker-unreachable"` | The PTY worker socket connect or write failed. The durable envelope the caller already wrote is the recovery record. |
| (inject error string) | `inject_into_pty` returned a non-connectivity error (e.g. body too large for the 16 MiB inject cap). The reason from the primitive flows through unmodified. |

**RPC error responses** (these are hard caller errors, not demotable conditions):

| Error code | Meaning |
|---|---|
| `AgentNotFound` | No registry entry for the given name. The caller should have validated first; this is a logic error. |
| `InvalidParams` | Missing or invalid `name`, `body`, or `from_name`; or body exceeds the 16 MiB daemon-side cap. |

The split between `delivered: false` (Ok result) and RPC error is deliberate. An unknown agent is always a caller logic error - treat it as `AgentNotFound`. A gate demotion or connectivity miss is a demotable condition: the caller's durable envelope is already on disk and the message is not lost, so returning Ok with `delivered: false` lets the caller log and continue without special error-handling.

## inject_into_pty primitive

`inject_into_pty` (daemon.rs) writes a framed message into a worker's PTY socket. The body size cap at the daemon side is 16 MiB (`MAX_INJECT_BODY_BYTES`); the Python CLI enforces 1 MiB (`_SEND_MAX_BODY_BYTES`) before the RPC is even called, so the daemon cap is a belt-and-suspenders guard.

The bytes written to the PTY worker socket are:

```
ESC[200~  <provenance-container>  ESC[201~  CR
```

`ESC[200~` / `ESC[201~` are the bracketed-paste guard sequences (xterm DEC mode 2004). Bracketed paste is required (AC4-EDGE from the bus design) because multi-line message bodies must land as a single paste event, not as line-by-line input that would submit on each newline. The trailing CR submits the paste after the closing bracket.

The write goes to the worker socket using the `bytes_b64` drive-shape path so arbitrary control sequences survive JSON encoding through the worker's IPC layer.

## Provenance container

Both the Python socket path (claude) and the Rust PTY injection path (codex/gemini) wrap the message body in the same container tag before delivery:

```
<cross-session-message from-name="<escaped-sender>">
<message body>
</cross-session-message>
```

The Python side (`cli/src/fno/agents/providers/claude.py`, `_build_envelope`) uses `html.escape(from_name, quote=True)` for attribute escaping and then wraps the result in a JSON `{"type":"user","message":{"role":"user","content":"<tag>"},"priority":"next"}` envelope for the socket protocol.

The Rust side (`daemon.rs`, `inject_into_pty`) uses `xml_attr_escape(from_name)` for the same attribute and writes the container text directly into the bracketed-paste frame. `xml_attr_escape` escapes the same five characters as Python's `html.escape(quote=True)`, including single quotes as `&#x27;`, so the container is byte-identical across the socket and PTY transports. Keep the two escapers in lockstep if either side changes.

The container tag identifies the sender as a peer, not as the operator. A receiving model that sees `<cross-session-message from-name="orchestrator-alpha">` knows it is getting a peer message and responds in a directed style rather than treating the injection as a user interrupt or an operator instruction. This is the anti-injection framing rationale from the design doc.

## Injection gate

Before attempting PTY injection for codex or gemini, the daemon checks the per-provider injection gate. The gate answers whether that provider's TUI queues mid-turn typed input safely - without interrupting the running turn or concatenating with existing composer input. A provider that fails this check would silently corrupt the peer's turn, so the gate defaults conservative.

**The `InjectionGate` enum** (daemon.rs):

```rust
pub enum InjectionGate {
    Passed,      // gate passed: PTY injection is safe for this provider
    Failed,      // empirical check ran and failed
    Unverified,  // gate not yet verified: demotion required
}
```

**The gate file** lives at `~/.fno/agents/injection-gate.json` (next to `registry.json` in the agents home directory, overridable via `FNO_AGENTS_HOME`). Schema:

```json
{
  "v": 1,
  "providers": {
    "codex": {
      "status": "passed",
      "checked_at": "2026-06-07T11:30:00Z",
      "method": "manual",
      "notes": "verified 2026-06-07 on codex 1.x"
    }
  }
}
```

`status` is one of `"passed"` or `"failed"`. `method` is `"manual"` for attestations written via `fno agents gate --record`; future automated probes would write a different method string.

**Conservative posture.** The gate resolver maps every ambiguous condition to `Unverified`:

- Absent file -> `Unverified`
- Malformed JSON -> `Unverified`
- `"v"` field missing or not equal to `1` (unknown schema version) -> `Unverified`
- Provider key absent from `"providers"` -> `Unverified`
- Status value not `"passed"` or `"failed"` -> `Unverified`

`Unverified` and `Failed` both demote to durable. The distinction exists so the event trail can separate "empirically confirmed bad" from "never checked". A probe that runs and finds no reliable signal returns `inconclusive` and writes nothing; the gate file is never flipped to `passed` by a probe.

**Locked Decision 9:** claude peers are never PTY-injected. The daemon checks `entry.provider == "claude"` before the gate read and returns `delivered: false, reason: "claude-routes-via-socket"` immediately. The Python dispatcher handles claude delivery via `send_to_session` (messaging socket) or `ask_followup_via_mcp` (MCP channel) and never calls `agent.deliver` for claude names.

**Atomic writes.** The `agent.gate_check` RPC (manual attestation mode) reads the existing gate file, merges the new provider entry, writes to a `.json.tmp` sidecar, then renames atomically. A partial write does not corrupt the existing records.

## Events

The daemon emits events to the shared events log on every deliver outcome.

**`agent_deliver_injected`** - PTY injection succeeded:

```json
{"name": "<agent>", "from_name": "<sender>", "provider": "<p>"}
```

**`agent_deliver_demoted`** - delivery demoted to durable (gate not passed, worker unreachable, or inject failure). The `reason` field matches the `reason` in the RPC result:

```json
{"name": "<agent>", "from_name": "<sender>", "provider": "<p>", "reason": "<reason>"}
```

The event reason and the RPC result reason are identical (sigma-review finding F3: they must agree so the event trail matches what the caller saw).

On the Python side, `dispatch_send` emits two events around the delivery attempt:

**`agent_send_started`** - envelope written, delivery about to be attempted:

```json
{"name": "<agent>", "provider": "<p>", "msg_id": "msg-<8hex>"}
```

**`agent_send_done`** - delivery attempt complete:

```json
{"name": "<agent>", "provider": "<p>", "msg_id": "msg-<8hex>", "delivery": "hosted|durable"}
```

Both Python events carry the dispatch context envelope (request_id, caller attribution, transport) set by `build_context` before the delivery attempt, matching the pattern established by `dispatch_ask`.

## What Group 3 changes

Group 3 of the cross-agent bus epic swaps the inbox store backing from the current per-recipient markdown thread files to a single global JSONL bus log (`~/.fno/bus/messages.jsonl`). The `write_new_thread` call in `dispatch_send` (step 4c, the durable envelope write) becomes a write to that log. The `send` verb's call sites, CLI flags, stdout contract, and delivery tier logic do not change - only the storage layer underneath the store API rotates.
