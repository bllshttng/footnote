# Rust client: claude `ask` (client-side `claude --bg` path)

## What this is

The Rust client (`crates/fno-agents`) can handle the `ask` verb for **claude** agents itself, replicating Python's `claude --bg` path byte-for-byte, instead of routing through the daemon. It lives in `crates/fno-agents/src/claude_ask.rs` and is wired into `bin/client.rs::maybe_run_claude_ask`.

## Why the daemon is the wrong shape for claude

`ask` was the last `fno agents` verb still owned by Python (`PYTHON_AGENT_VERBS` in `cli/src/fno/agents/rust_runtime.py`). Codex and gemini agents are PTY-managed by the Rust daemon, but `claude` is a `claude --bg` shellout: `ClaudeProvider.as_pty()` is `None`. A daemon-routed claude `ask` hits "worker not reachable". `claude --bg` is self-supervised — it runs its own background daemon, a rendezvous Unix socket, and a transcript/state dir under `~/.claude/jobs/<short-id>/`. There is nothing for the fno daemon to PTY-manage.

So the Rust **client** talks to claude's own session machinery directly, the same way Python's `providers/claude.py` + `providers/_claude_session_registry.py` + the `dispatch.py` ask path do.

## The path

- **Create** (`bg_create`): shell `claude --bg --name <name> <message>` (message via stdin past a 200KiB argv threshold), parse the `backgrounded · <8hex> · <name>` stdout, persist a registry row. Stdout: `<short_id>\n`.
- **Follow-up** (`ask_followup`): `locate_session` (scan `~/.claude/sessions/*.json` for the bg session with a live socket) → 250ms liveness probe → capture the state.json/timeline baseline BEFORE sending → send the **BG8 envelope** over the rendezvous AF_UNIX socket → `wait_for_reply` polls `state.json` for a post-baseline terminal transition, preferring `output.result`, falling back to the `timeline.jsonl` tail. Stdout: the reply verbatim (no added newline).
- **Orchestration** (`dispatch_claude_ask`): input validation (exit 2), a per-agent flock (`fs2`, cross-language-compatible with Python's `fcntl.flock`), create-vs-followup, registry status stamping (`live`/`orphaned`), exit-code map (1/2/11/12/13/15), and `events.jsonl` emission (`agent_ask_done`, `agent_followup_started|done|failed`).

## Byte-parity

Byte-parity with Python on observable behavior (stdout, exit code, the BG8 envelope bytes, events.jsonl fields) is the contract. Two Python-specific encodings are hand-rolled to match: `json.dumps(ensure_ascii=True)` (non-ASCII → `\uXXXX`, fixed key order) for the envelope and event lines, and `html.escape(quote=True)` for `from_name`. `claude_ask_parity.rs` pins the byte-critical surfaces (envelope, `parse_short_id`, reply extraction) against the **real** `fno.agents.providers.claude` so Python-side drift is caught (skips when python3 is unavailable).

## Scope boundary

This node delivers the **capability** (exercisable under `FNO_AGENTS_RUNTIME=rust` force-mode), not the auto-route flip.

Auto-routing landed in a follow-up ("Finish ask auto-routing"): provider-conditional, claude+codex by default, gemini stays Python. See [`fno-agents-ask-auto-route.md`](./fno-agents-ask-auto-route.md) for the resolution logic, the `RUST_CLIENT_ASK_PROVIDERS` set, and the `FNO_AGENTS_DEBUG_ROUTING` observability env var. The same node also added the codex `ask` Rust client port (`codex_ask.rs`).

Still deferred:

- Gemini `ask` Rust client port + unconditional flip.
- The MCP-channel transport path (US6: `ask_followup_via_mcp` / `mcp_channel_reachable`) — socket is the functional fallback.
- The success-event `EventContext` envelope.
