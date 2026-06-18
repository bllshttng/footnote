# fno-agents

Rust PTY supervisor substrate for footnote multi-CLI agents (Phase 6, backlog `ab-a09e1eaf`).

This crate is the substrate half of the Phase 6 design (`~/your-vault/internal/fno/design/2026-05-22-abi-pty-supervisor-and-drive.md`). It gives codex / gemini (and future OpenCode) agents the persistent-session, attach/detach, drive UX that Claude has. Python `fno agents <verb>` shells into the daemon/client binary; the daemon talks to per-agent workers over Unix sockets.

## Wave status

Waves 1-3 have landed: foundation + PTY substrate (W1), provider abstractions (W2), and the supervisor daemon + IPC + worker shim (W3), all built on **Wave 0's empirical outcome**.

Wave 0 (`cli/scripts/smoke/pty-survival/`) proved that a child on a PTY whose master the supervisor owns is SIGHUP'd and dies the instant the master closes (direct daemon-owned PTY does **not** survive daemon restarts). The locked outcome is **Outcome B**: a per-agent worker process owns the PTY master and outlives the daemon; the daemon reconnects to workers over their sockets on restart. See `cli/scripts/smoke/pty-survival-decision.md`.

Accordingly, the substrate here is written **worker-side**: `PtySession` is what a worker owns.

### Shipped in Wave 1

| Module | Contents |
|---|---|
| `lib.rs` | `ShortId`, `AgentStatus` (LD10), `ParsedEvent` (LD9 sealed enum), `MonotonicTimestamp` (LD17 + suspend-divergence pitfall) |
| `pty.rs` | `PtySession` (portable-pty spawn, worker-side master ownership) + `BoundedRing` drainer (LD31, 1MB default, drop-oldest) |
| `write_queue.rs` | `WriteQueue` bounded-backpressure stdin queue + `WriteMsg` enum |
| `supervisor.rs` | `RestartPolicy` state machine + `HARD_FAILURE_CEILING = 10` (LD36) |
| `readiness.rs` | `ReadinessDetector` trait + `UnknownReadinessSignal` (Open Q #9: no byte-count fallback) over a `ScreenView` seam |

### Provider layer (added on top of the substrate)

- `provider.rs` — `Provider` + `ProviderWithPty` traits. `as_pty()` returns `Option`, so `ClaudeProvider` (shellout to `claude --bg`, not PTY-managed) is distinguished from `CodexProvider` (JSONL stream) / `GeminiProvider` (single JSON blob) in the type system. `create_argv` / `resume_argv` mirror the validated Python adapters; `reachability` is tri-state and authoritative (codex session-index membership; gemini cwd-pinned short-prefix + full-UUID content check), never reporting a dead session live.
- `claude_ask.rs` — client-side `claude --bg` ask path (ab-cc926b4e). Because `ClaudeProvider::as_pty()` is `None`, the daemon cannot manage claude; the **client** replicates Python's `providers/claude.py` ask path directly (create + socket follow-up + reply extraction) with byte-parity. Wired via `bin/client.rs::maybe_run_claude_ask`. See `docs/architecture/fno-agents-claude-ask-rust.md`.
- `envelope.rs` — structural anti-injection `Envelope`. The user message is JSON-escaped, so the framing cannot be forged by message content (a hostile payload stays inside the `msg` field).
- `readiness.rs` — per-CLI `CodexReadinessDetector` / `GeminiReadinessDetector` over the `ScreenView` seam. Conservative: ready only on a positive prompt-glyph signal in the bottom status region, never a byte-count guess; rejects the gemini "Waiting for auth" false-ready.
- `screen.rs` — terminal-grid construction behind the `ScreenView` seam.

### Terminal-emulator crate: `vt100`, not `alacritty_terminal`

`screen.rs` uses `vt100`. The substrate design named `alacritty_terminal`, but its 0.26 release drags in a large transitive tree (winit-adjacent + windows crates) inappropriate for a lean, crates.io-distributed crate, while `vt100` (arrayvec + unicode-width + vte) exposes exactly the grid read the seam needs (`Screen::contents` + `Screen::cursor_position`). The `ReadinessDetector` trait depends only on `ScreenView`, so the emulator crate is reachable solely through `screen.rs` — swapping it touches one file.

### Shipped in Wave 3 (daemon + IPC + worker shim)

The supervisor layer on top of the substrate. Three binaries compile from this crate: `fno-agents` (client), `fno-agents-daemon`, `fno-agents-worker`.

| Module | Contents |
|---|---|
| `protocol.rs` | Length-prefixed (4-byte LE `u32`) JSON-RPC over the Unix socket; `agent.*` / `channel.*` namespace dispatch; `MAX_FRAME_BYTES` guard (malformed frame -> structured error, never a daemon crash). The drive WebSocket upgrade is a documented Wave 4 seam. |
| `events.rs` | Operator-facing `events.jsonl` emitter; `O_APPEND` open-write-close (atomic per sub-`PIPE_BUF` line); 500B payload cap -> `event_payload_too_large` meta-event (never silent); size rotation. |
| `state.rs` | `registry.json` (v3) + per-agent `state.json` (v1); flock-protected (`fs2`, `flock(2)`) atomic read/modify/write via a `.lock` sidecar that is the canonical cross-language lock target; `PtyState::take_active_drive` co-locates the recovery read-before-clear ordering invariant. |
| `paths.rs` | `~/.fno/agents/` layout + 0700/0600 perm helpers + worker-socket discovery scan. |
| `daemon.rs` | Six-state lifecycle (event on entry); socket bind with 0700/0600 fstat-verify + lazy-start race resolution + flock self-test; 7-step startup recovery (`drive_crashed` before clear, orphan-dir archive, dead-PID reap); `agent.spawn/ask/list/status/stop/rm/reconcile` + `channel.register/unregister/push`; idle lazy-exit; zombie reaping; UUIDv4 from urandom. |
| `worker.rs` + `bin/worker.rs` | Per-agent PTY shim (**Outcome B**): owns the PTY master, runs in its own process group, ignores SIGHUP, outlives the daemon. Serves `worker.write/snapshot/status/resize/shutdown`; emits an `agent_exited` event and flips the registry to `exited` on exit. |
| `client.rs` + `bin/client.rs` | Lazy-start the daemon, connect, forward a verb; map error code -> exit code. |

Real-subprocess tests (`tests/daemon_e2e.rs`) prove the load-bearing claims: an agent survives a daemon `SIGKILL` and a restarted daemon reconnects via socket scan; agent exit is observable. `tests/flock_interop.rs` proves cross-language `flock(2)` coordination with Python `fcntl.flock` (the `cross_language_flock_test` kill criterion).

### Not yet present (arrive with their consumers)

- **Drive WebSocket surface** (Wave 4) — `agent.drive` upgrade, sentinel detach, gate-hardening.
- **Lifecycle-verb polish** (Wave 5) — full tri-state reconcile probing, per-agent ask serialization.

### Distribution (Wave 6)

The crate is publishable (`publish = true`) and ships three ways — platform wheels with
the binary bundled, standalone GitHub Release tarballs, and `cargo install fno-agents`.
Once installed, this binary is the **default** `fno agents` runtime for the daemon-native
verbs (`spawn`, `status`, `drive`, `*-channel`); the verbs that share an established Python
contract stay on Python until the client reaches parity (`FNO_AGENTS_RUNTIME=python` pins
everything to Python; `=rust` forces the binary for every verb). Build matrix and the
gated live-publish runbook:
[`docs/distribution.md`](../../docs/distribution.md). Nothing is published yet (machinery
only); the publish workflows are gated behind manual dispatch + secrets.

## Dependency choices (Claude's Discretion #1)

- `portable-pty = "0.8"` — pinned to the version Wave 0 validated (0.9.0 available; AgentRelay's pin was unavailable, the reference repo is absent locally). Re-evaluate at Wave 2.
- `serde` / `serde_json` — state-file + cross-language schema serialization.
- `thiserror` — typed error enums; the crate avoids `unwrap()` in library code by policy (an anti-pattern called out in the AgentRelay investigation).
- `tracing` — log-grade observability (distinct from event-grade `events.jsonl`).
- `libc` — `clock_gettime(CLOCK_BOOTTIME)` (Linux) and `mach_continuous_time` (macOS) for the count-during-sleep `MonotonicTimestamp`.

- `vt100` — terminal-grid parsing behind the `ScreenView` seam (added with `screen.rs`). Chosen over `alacritty_terminal`; see "Terminal-emulator crate" above.

- `tokio` (rt-multi-thread, net, io-util, sync, time, signal, macros, process) — the daemon's async runtime + Unix-socket IPC (Wave 3). The substrate stays runtime-agnostic; only the daemon/worker/client depend on it.
- `fs2` — `flock(2)` advisory locking, interoperable with Python's `fcntl.flock` for cross-language state-file coordination (Wave 3, US6.12).

`tokio-tungstenite` (drive WebSocket, Wave 4) and `flate2` are NOT yet dependencies; they arrive with their consumers. `alacritty_terminal` is intentionally not used.

## Tests

- Unit tests are inline per module (`ring overflow`, `restart ceiling`, `write-queue backpressure`, `monotonic clock`, `readiness no-guess`, framing roundtrip, event-cap, recovery ordering, socket perms, the worker RPC surface).
- `tests/pty_spawn.rs` is real-subprocess (Discretion #5): it spawns actual `bash` / `cat` children on real PTYs and asserts output drains and input round-trips. No monkeypatching.
- `tests/daemon_e2e.rs` (Wave 3) spawns the real `daemon` + `worker` binaries: proves Outcome B survival across daemon `SIGKILL`, recovery reconnect, spawn collision, per-project list, channel roundtrip, and exit observability.
- `tests/flock_interop.rs` (Wave 3) proves cross-language `flock(2)` coordination against `python3` (`fcntl.flock`); skips cleanly when `python3` is absent.

```bash
cargo test -p fno-agents
```
