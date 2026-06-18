# fno agents drive surface (Phase 6 Wave 4)

The `drive` surface lets an operator step into a live PTY-managed agent: take it over interactively, watch it read-only, or step through input under confirmation. It is the user-facing payoff of the Phase 6 Rust supervisor (`crates/fno-agents/`). This doc covers the wire protocol, the daemon session machine, the modes, the heartbeat/takeover resilience, and the gate-hardening seam.

## Wire sequence

Drive does not fit the daemon's one-request/one-response shape, so `serve_connection` intercepts `agent.drive` before the normal dispatch and hands the owned Unix stream to `drive::handle_drive`, which keeps it open for the session.

1. Client connects to `supervisor.sock` and sends a length-prefixed `agent.drive` JSON-RPC request: `{name, mode}`.
2. Daemon validates (see below), sets the `state.json` drive window for controlling modes, emits `drive_attached` **before any frame** (the ordering invariant the stop hook depends on), and acks with `{session_id, mode}`.
3. Both sides upgrade the same stream to a WebSocket (LD21: tokio-tungstenite standard handshake; the bogus Origin/Host on a Unix socket is irrelevant).
4. The client sends an initial `{"t":"resize","rows","cols"}` control frame (LD18); the daemon waits up to 2s then defaults to 24x80.
5. Bidirectional frames flow until detach.

A rejection (bad status, not PTY-managed, caps hit) is returned as a structured error response on the raw stream, with no upgrade.

### Frame vocabulary

| Frame | Direction | Meaning |
|---|---|---|
| Binary | client to daemon | raw keystrokes (forwarded to worker stdin; suppressed for watch) |
| Binary | daemon to client | live PTY output (streamed via the worker's `read_since` cursor) |
| Text `{"t":"resize","rows","cols"}` | client to daemon | initial handshake + on SIGWINCH |
| Text `{"t":"ping"}` / `{"t":"pong"}` | both | heartbeat |
| Text `{"t":"detach","reason"}` | client to daemon | clean sentinel exit |
| Text `{"t":"dropped"}` | daemon to client | PTY ring overflowed (output gap) |
| Text `{"t":"server_event","event":"child_exited"}` | daemon to client | the agent's child exited |

## PTY output streaming

The daemon talks to the per-agent worker over fresh per-RPC connections, so multiple drive/watch clients each poll independently with no worker-model change. Output uses an incremental cursor rather than diffing full snapshots:

- `BoundedRing::total_written() = dropped + len` is a monotonic absolute offset.
- `worker.read_since {cursor}` returns `{bytes_b64, next_offset, gap, child_alive}`. `gap` is true when the cursor pointed at bytes the ring has since dropped, so the client gets a `dropped` notice rather than silently spliced output.
- Keystrokes ride `worker.write {bytes_b64}` (base64, because raw keystrokes carry control bytes and arbitrary non-UTF-8 a JSON string cannot).

## Modes

| Mode | Input | Authority window | Notes |
|---|---|---|---|
| `interactive` | forwarded raw | yes | full takeover |
| `watch` | suppressed (client) + rejected (server, `drive_watch_input_rejected`) | no | read-only; many per agent up to `max_watchers_per_agent` (5) |
| `step` | per-line, confirmed | yes | each confirmed line emits `drive_keystroke_stepped` |
| `paranoid` | per-byte, confirmed | yes | stricter step |

Watch is the sole read-only carve-out (LD24/29). `paranoid` is a stricter `step`, so it hardens like one (treating it as read-only would leave an authority hole).

## Concurrency: the drive table

The daemon holds an in-memory `DriveTable` keyed by agent `short_id`: at most one controlling driver (interactive/step/paranoid) per agent, plus watchers. It enforces a single controlling driver per agent, `max_concurrent` controlling drivers across all agents (10), and the per-agent watcher cap. The controlling driver is mirrored into the agent's `state.json` drive window (the stop-hook authority signal); watchers are in-memory only.

## Heartbeat, takeover, recovery

- **Heartbeat watchdog**: a per-session task force-closes a driver whose client stopped pinging past the timeout (`config`/`FNO_AGENTS_DRIVE_HEARTBEAT_MS`, default 10s), emitting `drive_detached{reason:"heartbeat_lost"}`. Timing uses the monotonic count-during-sleep clock (LD17, `MonotonicTimestamp`) so wall-clock skew / laptop sleep does not falsely keep or expire a window.
- **Stale-driver takeover**: a new driver arriving while the existing one has been idle past `STALE_DRIVER_IDLE` (30s) evicts it (`drive_takeover_after_stale`) and proceeds.
- **Daemon-restart recovery**: startup recovery reads each stale drive window, emits `drive_crashed{reason:"daemon_restart"}` BEFORE clearing it, so events.jsonl reflects reality from the first served request.

Every drive session ends with EXACTLY ONE of `drive_detached` (clean / forced / heartbeat) or `drive_crashed` (daemon restart). No silent-leak path.

The close signal is a `watch` channel and the pump-completion signal a `oneshot`: both latch, so a forced close or pump exit between the session's select iterations is still observed (a `Notify` would lose the wakeup and hang an idle session).

## The client: `fno-agents drive`

`fno-agents drive <name> [--watch|--step|--paranoid]` runs a single select loop bridging stdin and the WebSocket:

- Raw terminal mode via an RAII `termios` guard (a no-op when stdin is not a TTY, so headless/piped use still works).
- The `Ctrl-\ d` detach sentinel (LD20, `FNO_AGENTS_DRIVE_SENTINEL`-overridable) is detected client-side, holding a lone lead byte across reads so a split sentinel is still caught and a lead-not-followed is forwarded intact. Data preceding the sentinel in the same read is forwarded before detaching.
- Step/paranoid buffer keystrokes into confirmation units gated by a `y/N` prompt.
- SIGWINCH sends a resize; a 3s ping keeps the watchdog satisfied; a drive-ended stderr summary reports reason / duration / keystrokes / step-confirmed.

## Gate-hardening

While an operator holds an `interactive`/`step`/`paranoid` window, the operator (not the LLM) authored the bytes flowing into that agent, so gate signals seen during the window are operator-initiated, not LLM authorship (LD3). The detection primitive is `fno.agents.drive_authority`:

- `is_drive_authority_active()` / `active_drive_sessions()` read each agent's `state.json` drive window; watch never counts.
- `fno agents drive-authority [--json]` exposes it to bash hooks (exit 0 when active).

Liveness is the daemon's job (the watchdog evicts stale drivers; recovery clears leaked windows), so a present authority window is an authoritative "an operator is driving now" signal. The audit events the matrix relies on ship in the daemon.

**Wave 8 enforcement.** The detection primitive above is now wired into the Python stop-hook layer. `scripts/lib/drive-authority.sh::drive_authority_active()` shells out to `fno agents drive-authority` (fail-open: an absent/erroring `fno` reads as "no window" so daemon-less sessions are never blocked). The stop hook (`hooks/target-stop-hook.sh`) consults it on the `<promise>` path only: a `<promise>` emitted while an authority window is open is logged as `promise_forged_during_drive`, the completion audit is skipped, and the session stays `IN_PROGRESS` (LD3/LD29). `session_satisfied` auto-complete is intentionally not gated - it comes from constrained external sources (check_pr / pr_merge / CI), not operator keystrokes. `BLOCKED`-during-drive needs no new code: `BLOCKED` is already hook-written-only (the typed-blocker invariant reverts any non-hook author). The remaining matrix cells - PreToolUse refusal of gate-boolean edits during a drive, and `operator_initiated` audit-tagging of allowed actions (`fno backlog done` / `fno gate set` / artifact edits) - are captured as follow-up carveouts; the load-bearing authority decision (the promise) is enforced here. Integration test: `tests/hooks/test_drive_authority_enforcement.sh` (Open Question #10) exercises the detection matrix across all modes plus the refusal decision.

**Session scoping by per-agent identity.** `fno agents drive-authority` reports windows machine-wide (every agent under the shared store, across every project). The original seam treated any open window as "active here," so an operator driving an unrelated agent hung a finished `/target` session whose `<promise>` the stop hook then refused. The guard now scopes to THIS session by agent identity: the PTY worker stamps the agent's `short_id` into the child environment as `FNO_AGENTS_SELF_SHORT_ID` (`crates/fno-agents/src/worker.rs`), which the child (claude/codex) and the Stop / graph-write-protect hooks it spawns inherit. `drive_authority_active()` fires only when an open authority window targets that `short_id`. A plain terminal session has no stamped id, so no PTY of ours is drivable and it is never blocked. Identity beats the interim cwd-scoping on both counts that broke it: multiple named agents can share one cwd (cwd over-blocks a co-located terminal `/target` or second worker), and a `config.state_dir` / `FNO_AGENTS_HOME` override left the registry unresolved and fail-opened; identity needs no registry read at all. Fail-open is preserved: if `fno`/`jq` is absent or the env var does not propagate, the guard reads inactive (safe for "no hang," at the cost of not refusing an operator-typed promise on a genuinely-driven session). The integration test's seam (Section B) and graph-write-protect cells (Section E) assert the identity contract, including the cross-agent case (a drive on a different `short_id` must not block this session). End-to-end propagation of `FNO_AGENTS_SELF_SHORT_ID` through the full chain (PTY worker -> claude child -> bash Stop / graph-write-protect hooks) was verified live on 2026-05-31 against Claude Code 2.1.156: both a SessionStart hook and a Stop hook observed the exact value stamped by the worker, confirming LD3 is effective and the daemon `drive->session_id` fallback is unnecessary. The Rust unit tests in `crates/fno-agents/src/worker.rs` (`build_child_command_stamps_self_short_id` / `build_child_command_stamps_fno_agents_home`) lock the worker-side stamp against silent removal; `tests/hooks/verify-self-short-id-propagation.sh` is a committed manual verifier for the claude->hook leg.

## Tests

- Rust unit: ring cursor, drive mode/authority, control-frame parse, status eligibility (LD28), sentinel detector (incl. split / double-lead / not-followed), step buffer (per-line / multi-line / per-byte).
- Rust real-subprocess e2e (`tests/drive_e2e.rs`): full WS keystroke -> PTY -> output roundtrip with window + event lifecycle; second-driver Busy + non-PTY/ghost rejection; heartbeat watchdog eviction; watch input rejection + no-window + multi-watcher.
- Python: `drive_authority` detection across modes + corrupt-state skip + the CLI verb's exit codes.
- Bash (W8): `tests/hooks/test_drive_authority_enforcement.sh` - operator-authority matrix integration test. Real-CLI detection across interactive/step/paranoid (active) and watch/none/corrupt (inactive); the `drive_authority_active` seam (exit-code passthrough + fail-open when `fno` absent); and the stop-hook refusal decision (promise during drive refused; promise / `session_satisfied` without drive honored). Wired into `cli-ci`.
