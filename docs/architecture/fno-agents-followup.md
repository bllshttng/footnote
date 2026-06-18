# fno agents — follow-up via messaging socket

Calling `fno agents ask <existing-name> "<message>"` against a running Claude agent delivers the message to its `claude --bg` supervisor over the supervisor's messaging socket and prints the agent's reply on stdout. Re-using a name routes to this follow-up flow instead of failing as a duplicate.

Parent: [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md). User guide: [../guides/fno-agents-ask-followup.md](../guides/fno-agents-ask-followup.md).

## Why the messaging socket

`claude --resume --print` operates on saved transcripts, not running supervisor sessions; using it for follow-up would silently fork the conversation (confirmed by reverse-engineering claude 2.1.143).

Follow-up instead uses the `messagingSocketPath` Unix-domain socket that the supervisor exposes in `~/.claude/sessions/<pid>.json`. This is the same channel the TUI's peek-reply panel uses, and the recipient framing (`<cross-session-message from-name="...">`) carries built-in anti-injection semantics.

Three external surfaces are read at runtime, all reverse-engineered:

| Source | Owner function in claude 2.1.143 | What we read |
|---|---|---|
| `~/.claude/sessions/<pid>.json` | `IE7` | `messagingSocketPath`, `jobId`, `kind`, `sessionId`, `cwd` |
| `~/.claude/jobs/<short-id>/state.json` | `state.json` writer | `state`, `updatedAt`, `output.result`, `intent` |
| `~/.claude/jobs/<short-id>/timeline.jsonl` | timeline emitter | append-only `{at, state, detail, text}` rows |

The protocol over the AF_UNIX socket per `BG8`/`CE7`/`Ag5`: SOCK_STREAM, single-shot per message, JSON envelope `{"type":"user","message":{...},"priority":"next"}` newline-terminated, no ack, 5-second write timeout, 250 ms connect probe for liveness. Schema drift in any of those surfaces is caught by the tests' real-socket and real-state.json fixtures.

## Module map

| Module | Role |
|---|---|
| `cli/src/fno/agents/providers/_claude_session_registry.py` | `locate_session`, `read_state_json` (retry-on-rename), `read_timeline_tail`, `TERMINAL_STATES`, `SessionLocator`, `StateSnapshot` |
| `cli/src/fno/agents/providers/claude.py` | `send_to_session`, `liveness_probe`, `wait_for_reply`, `ask_followup`, `ProviderOrphanError`, `ProviderSocketError`, `ProviderTimeoutError`, `OrphanReason` |
| `cli/src/fno/agents/dispatch.py` | `DispatchAskResult`, `DispatchKind`, `_followup_path`, `_stamp_status`, `_validate_from_name` |
| `cli/src/fno/agents/cli.py` | `--from-name` option, kind-discriminated stdout |
| `cli/src/fno/agents/registry.py` | `AgentEntry.status` + `last_message_at`, `AgentStatus` Literal, `KNOWN_STATUSES`, schema-version read-time synthesis |

`lock.py` (`hold_agent_lock`), `events.py` (`emit`), and `providers/base.py` (`ProviderResult`) are touched as readers only. The per-agent flock model is unchanged from the create path.

## Lifecycle of one follow-up call

1. `cmd_ask("existing-name", "msg", --provider claude, --from-name "footnote")` → `dispatch_ask`.
2. `dispatch_ask` validates name + message + from_name, then acquires `hold_agent_lock(name)`.
3. Under the lock: load the registry, run `select_provider` (catches `--provider gemini` on a Claude entry), then route on `existing is not None` → `_followup_path`.
4. `_followup_path` emits `agent_followup_started`, then `claude.ask_followup`:
   1. `locate_session(short_id)` scans `~/.claude/sessions/`. Misses become `ProviderOrphanError(reason="not-found")` or `"socket-null"` after `_classify_orphan_reason` re-reads to distinguish.
   2. `liveness_probe(sock_path)` is a 250 ms connect probe. Failure → `ProviderOrphanError(reason="liveness-failed")`.
   3. Capture `baseline_updated_at` from state.json + `timeline_offset` from timeline.jsonl **before** the send. This is the load-bearing invariant: a stale pre-send `output.result` must not impersonate the new reply.
   4. `send_to_session` builds the envelope, AF_UNIX-connects, `sendall + newline`, closes. Any connect/write/**close** error raises `ProviderSocketError`.
   5. `wait_for_reply` polls state.json (500 ms cadence by default, `--timeout` ceiling — 600 s by default). Exit condition: `updatedAt > baseline AND state ∈ TERMINAL_STATES`. Reply preference: `state.output.result` first, `read_timeline_tail` fallback when `output.result` is empty.
5. On success: `_followup_path` bumps `last_message_at` and `status="live"` via `update_registry`, emits `agent_followup_done`, returns `DispatchAskResult(kind="followup", short_id, reply)`.
6. cmd_ask writes `result.reply` verbatim to stdout — no trailing newline added; the recipient's own newline is preserved.

## Failure modes and exit codes

| Path | Exit code | stderr (key phrase) | Registry side-effect |
|---|---|---|---|
| Happy path follow-up | 0 | (silent) | `status="live"`, `last_message_at=now` |
| `not-found` orphan | 13 | `is not running (reason: not-found)` | `status="orphaned"` (best-effort) |
| `socket-null` orphan | 13 | `is not running (reason: socket-null; session is suspended)` + `claude attach` hint | `status="orphaned"` |
| `liveness-failed` orphan | 13 | `is not running (reason: liveness-failed)` + `claude attach` hint | `status="orphaned"` |
| Provider mismatch | 2 | `refusing to follow-up as provider=<x>` | unchanged |
| from_name unsafe | 2 | `must not contain XML-unsafe characters` | unchanged |
| Empty message | 2 | `message must be non-empty` | unchanged |
| Send failure (socket error) | 1 | `messaging socket error: <reason>` | unchanged |
| Poll timeout | 15 | `message sent but no reply within <N>s` + `fno agents logs` hint | `last_message_at` NOT bumped |
| Registry write fails post-send | 12 | `registry write failed: <reason>. NOTE: message was already delivered; do not retry.` | inconsistent; lock held as manual-cleanup signal |
| Lock timeout | 11 | `lock timeout for agent '<name>' after 30s` | unchanged |
| Ctrl-C during poll | ~130 | (Python's default KeyboardInterrupt traceback) | `last_message_at` NOT bumped, lock released |

The exit codes are deterministic and distinct so an LLM orchestrator can branch without parsing prose.

## What the recipient sees

The wrapped content arrives at the recipient with explicit `<cross-session-message from-name="footnote">…</cross-session-message>` framing, which claude's TUI renders with a "system notification, not user input" preface. This is a security feature (anti-injection) and a wording surprise — the recipient's reply will be more orchestrator-directed than a user-driven follow-up would yield.

## Concurrency

The per-agent flock (`hold_agent_lock`) wraps the entire `dispatch_ask` flow including the follow-up branch's send + poll loop. Two parallel `ask <same-name>` calls serialize at the agent-name level; the recipient agent sees both messages in flock-acquire order. The registry-wide flock inside `update_registry` makes the load + modify + write triplet atomic across different agent names. Lock order is fixed: per-agent first, registry-wide second — no deadlock risk.

## Schema migration

The registry schema version supports both the pre-follow-up shape (`load_registry` synthesizes `status="live"`, `last_message_at=None` in memory; does **not** mutate the on-disk file) and the current shape (round-trip preserves the new fields). The next write upgrades the on-disk file transparently. The provider check and shape check are unchanged — alien values still raise `RegistryVersionError`.

## Out of scope for this path

- Suspended-session wake (`messagingSocketPath: null` → daemon-backend respawn).
- codex / gemini follow-up — separate substrate; see [fno-agents-codex-provider.md](fno-agents-codex-provider.md) and [fno-agents-gemini-commands.md](fno-agents-gemini-commands.md).
- Streaming output (`--stream` flag tailing timeline.jsonl).
- The MCP channel server, the sanctioned long-term replacement for the reverse-engineered messaging socket — see [fno-agents-mcp-channel.md](fno-agents-mcp-channel.md).
