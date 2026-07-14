# Claude stream-json host lane (Group 1)

Status: shipped. The Group 1 worker substrate landed first; the
`send` switchboard (Group 2) and the `host`/`promote` front door (Group 3)
that drive this lane have merged, and the headless permission posture +
single-writer claim hardening closed out the epic.

## Why

codex and gemini are daemon-PTY-hosted: the daemon owns a PTY worker per session
and can drive it live. claude is different - it is a shellout (`claude --bg`),
async-reachable only. After the cross-agent bus epic, a claude session a human
started by hand registers as `idle` and there is no way to make it live and
drivable by name.

claude's `--input-format stream-json` (only valid with `-p`/`--print`) gives a
bidirectional, structured pipe: turns in, partial tokens + assistant message +
result + control requests out. Resuming an idle session by its full UUID
(`claude -p --resume <uuid> --input-format stream-json --output-format
stream-json`) reconstructs the transcript and yields a live thread. This is the
claude analog of the codex/gemini PTY lane: a NEW daemon IO substrate (a pipe)
alongside the existing PTY one, sharing the registry.

Cost note: `-p` draws a dedicated Agent SDK plan credit, isolated from
interactive limits (support article 15036540). It is a deliberate, opt-in lane,
not the default. Tests NEVER spawn real `claude -p` - they use a fake stream-json
emitter.

## Components (this group)

| Piece | Where | Role |
|---|---|---|
| `claude_session_uuid` field | `registry.py` `AgentEntry`, `state.rs` `RegistryEntry` | The full session UUID = the `--resume` target, distinct from the 8-hex `claude_short_id`/jobId (a 32-bit prefix, not collision-proof). Additive-optional, round-trips both languages. |
| `resolve_session_uuid` | `_claude_session_registry.py` | jobId -> full UUID, reading `sessionId` from `~/.claude/sessions/<pid>.json` REGARDLESS of socket state (an idle/socket-null session is exactly the resume target; `locate_session` skips those). |
| MCP reply-poll decoupling | `claude.py` `ask_followup_via_mcp` | Derives the jobs-dir directly from the short-id instead of via `locate_session`, so an idle session is not falsely orphaned. The MCP send routes through the sidecar and the reply is polled from the jobs-dir; neither needs the (dead) unix socket. |
| Single-writer claim guard | `claude.py` `acquire_session_writer_claim` / `release_session_writer_claim` | Before respawn: (1) refuse if the bg session is held live by another process (`locate_session` + `liveness_probe`); (2) acquire `fno claim session:<uuid>` (O_CREAT\|O_EXCL) so two concurrent adopts cannot both respawn one transcript. `claude --resume` does not self-guard. Reuses the existing `fno claim` primitive; session claims are host-global. |
| Frame parser | `stream_worker.rs` `parse_frame` | The stream-json discriminator: System / StreamEvent / Assistant / Result / UserEcho / Other / Malformed. A `--replay-user-messages` echo (`type:user`) is a DELIVERY RECEIPT, never the reply; a malformed line is skipped (never fatal). Pure + total. |
| Per-session stream worker | `stream_worker.rs` `StreamSession` + `run` | Owns the `claude -p --resume` child over stdin/stdout pipes spawned FROM THE RECORDED CWD (resume is cwd-scoped). A background thread parses stdout into a bounded frame log (gap-on-overflow); stdin is mutex-guarded so a turn never interleaves. Serves non-blocking RPCs over `<short_id>/worker.sock`. |
| Worker `--stream` mode | `bin/worker.rs` | Routes `fno-agents-worker --stream ...` to `stream_worker::run`. |
| Resume argv | `provider.rs` `claude_stream_json_resume_argv` | The exact `claude -p --resume <uuid> --input-format stream-json --output-format stream-json --include-partial-messages --replay-user-messages` flags. |

## Worker RPCs

The worker is single-client (only the daemon connects) and serves connections
serially, so one turn is in flight at a time.

| Method | Effect |
|---|---|
| `stream.write_turn {text}` | Writes a `{"type":"user",...}` turn to the child's stdin. |
| `stream.read_frames {cursor}` | Returns parsed frames from `cursor`, the next cursor, a `gap` flag, and `child_alive`. The consumer polls until a `result` frame closes the turn. |
| `stream.status` | child_pid / child_alive / exit_code. |
| `stream.shutdown` | Kills the child and ends the worker (-> `Exited`). |

## Outcome B (daemon-death survival)

Identical to the PTY worker. The daemon launches the worker in its own process
group and the worker binary ignores SIGHUP, so a daemon SIGKILL does not reach
the worker or its child. On daemon restart the recovery sweep rediscovers the
worker by scanning for `<short_id>/worker.sock` - the daemon holds no per-worker
state, so reconnect is lane-agnostic and works for stream workers with no new
daemon code. A child that dies on its own flips the registry row to `Orphaned`
and releases the single-writer claim (best-effort; PID-liveness + the daemon
reconcile are the backstops); a clean `stream.shutdown` flips it to `Exited`.

## Later groups (shipped)

The substrate above is now driven by work that has since landed:

- **Front door** - `fno agents promote/host --provider claude` (claude routed
  before `admit_promote` in the daemon's interactive `handle_spawn`;
  `spawn_claude_stream_lane`) - Group 3.
- **`send A->B` live switchboard** + `config.agents.a2a.*` toggle/ceiling +
  `fno agents watch` observe surface - Group 2.
- **Capability matrix** (`docs/harness-command-matrix.md`) + `config.agents.a2a.*`
  schema (`docs/provider-rotation.md`) - Group 3.
- **Headless `can_use_tool` permission posture** - the worker answers every
  `control_request{subtype:can_use_tool}` on the child's stdin from its stdout
  reader thread, so a headless adopted turn never hangs, under a default-deny
  posture: shell tools (Bash, ...) are never auto-approved, path-bearing tools are
  denied outside the canonicalized session cwd, and otherwise only bare
  `permissions.allow` rules pass.
- **Single-writer claim PID anchor** - the worker re-acquires its
  `session:<uuid>` claim with its own long-lived PID on startup (`fno claim
  acquire --pid`), so PID-liveness tracks the actual writer instead of the
  ephemeral `fno` process the daemon shelled pre-spawn.

The G2 + G3 architecture (switchboard, front door, permission posture, and the
two-layer single-writer model), along with the full design and BDD acceptance
criteria, is documented in the maintainers' vault.
