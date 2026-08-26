# Codex thread driver: the measured protocol surface

Why codex has no thread lane today, what a driver must speak, and what earns the capability bit. Measured against `codex-cli 0.149.1` on 2026-08-26.

A `thread` is an fno-layer construct. fno invents it and implements it over each harness's own resume primitive. No harness ships one. `bg` was only ever a claude subcommand. So this page does not ask whether codex has a thread feature. It answers what fno must build to own a codex thread lifecycle with no PTY.

Read this before you conclude that codex cannot have a thread lane. It can. Nothing has built one.

## The bit stays false, and that is not a formality

`cli/src/fno/agents/harness_capabilities.toml` holds `thread = false` for codex. The bit is evidence of a working driver with its own unattended journey test. It is never an aspiration. `harness_map.py`'s rule is that it is never inherited.

Everything below is protocol that exists. None of it is a lane fno ships. Do not flip the bit against this page. Flip it against a driver and the journey test at the foot of it.

## The two durable halves already exist

Codex persists every conversation as a rollout at `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<full-session-id>.jsonl`. The full session id is in the filename. The working cwd is in the body. `codex resume <full-id>` re-enters one with context intact.

The transcript half and the resume half are both on disk. What is missing is an fno-side driver that owns the lifecycle.

## Transport: no daemon required

`codex app-server` speaks JSON-RPC 2.0. The `"jsonrpc":"2.0"` header is omitted on the wire. Its default transport is newline-delimited JSON on stdin and stdout. A plain pipe child speaks the whole protocol. No PTY, no WebSocket and no daemon are involved.

That matters for a driver's dependency surface. The detached `codex app-server --remote-control` daemon is a second way in. It listens at `$CODEX_HOME/app-server-control/app-server-control.sock` over a WebSocket, using the standard HTTP Upgrade handshake. `crates/fno-agents/src/codex_inject.rs` binds to it. A driver does not need it.

Two transport traps are recorded here so nobody re-discovers them.

**The control socket file outlives the process that bound it.** Unless a process cleans up on exit, its unix socket inode stays. So an `exists()` probe reads healthy against a daemon dead for a day. Read the pid from `~/.codex/app-server-daemon/app-server.pid` instead. Probe that pid. Use its `processStartTime` as an incarnation token. Require the `initialize` handshake to return.

**`codex app-server proxy --sock <path>` is unreliable.** It accepted frames and returned nothing against a confirmed-live daemon, twice, with no stderr. The WebSocket lane to that same socket worked in the same session. Prefer stdio, or the existing WS client.

## What the protocol offers a driver

`codex app-server generate-json-schema --out <dir>` emits a version-exact schema bundle. On 0.149.1, `ClientRequest.json` enumerated 95 request methods. `codex_inject.rs` implements four of them.

Every duty a thread driver owes has a named RPC:

| Duty | RPC | Notes |
|---|---|---|
| create | `thread/start` | returns `threadId` and the rollout `path` |
| resume from disk | `thread/resume` | by `threadId`, by rollout `path`, or by `history`. Takes `cwd`, `sandbox`, `approvalPolicy`, `config`, `model` and `modelProvider` as typed params |
| drive a turn | `turn/start` | the response carries a `turnId` and is itself the delivery receipt, so no transcript growth-poll |
| structured output | `turn/completed` notification | `items[]` with `type`, `text`, `phase`, plus `status`, `startedAt`, `completedAt`, `durationMs` |
| steer mid-turn | `turn/steer` | requires `expectedTurnId`, a precondition that fails against a stale turn |
| interrupt | `turn/interrupt` | the terminal turn reads `status: "interrupted"` |
| enumerate the roster | `thread/list` | paged. Per row: `id`, `cwd`, `preview`, `status.type`, `path`, timestamps. Filters by `cwd` |
| enumerate hosted | `thread/loaded/list` | separates threads this app-server hosts from `notLoaded` |
| name a row | `thread/name/set`, `thread/metadata/update` | |
| review | `review/start` | returns a `turnId` and a `reviewThreadId` to read findings from |

Also present: `thread/fork`, `thread/rollback`, `thread/compact/start`, `thread/archive`, `threadSection/*`, `hooks/list`, `model/list`.

`turn/steer`'s `expectedTurnId` is worth dwelling on. A keystroke into a pane cannot assert which turn it steers. A driver can.

## `thread/list` is a roster with the right cwd

One stdio child paged `thread/list` to exhaustion. It returned 630 threads. 171 of those sat across 121 distinct worktree paths, each carrying its true working directory.

This bears on a known hazard. `~/.fno/agents/registry.json` records the spawn dir, not the working dir. An auto-resume driven off it drops workers onto the default branch in the shared checkout. That failure looks like success, which is worse than a visible error.

Codex's own roster does not have the defect. The cwd is in the rollout body and `thread/list` surfaces it. A driver reading `thread/list` needs no cwd repair and no join to fix.

## A pane worker is not a hosted thread

This is the finding most likely to be re-derived the hard way.

An app-server hosts only the threads it started or resumed. A codex pane is a separate `codex` process with its own rollout. The app-server never had a handle on it. Measured with a confirmed-live daemon and roughly forty codex panes alive, `thread/loaded/list` returned zero.

The consequence is an integrity hazard, not a capability gap. `review/start` names a `threadId`. So it can never target a pane worker, however healthy the daemon is. The documented pane fallback is separately refused while a task runs. With both routes shut, an honest agent falls through to writing its own attestation and calling it review coverage.

A driver-hosted thread closes this by construction. It appears in `thread/loaded/list`, and `review/start` against it returns a receipt.

## The fno-side gap is one lane

`crates/fno-agents/src/daemon.rs` is already the supervisor. It already has a startup recovery hook. Two facts about it:

- `recover()` skips a row whose `short_id` is empty. Codex rows carry no `short_id`, so every one is skipped.
- Recovery is a reconcile-and-GC pass. Its whole output is `RecoveryReport { inconsistent, archived_orphans, reaped_pids, recovered_drives }`. There is no resume branch for any harness.

The precedent to copy sits in the same file. `spawn_claude_stream_lane` holds an idle claude session as a stream thread over `claude -p --resume <uuid>`. Chat, switchboard and ask drive it. Codex needs that lane's counterpart. The app-server protocol is the better substrate for it, because it returns a turn id.

`crates/fno-agents/src/opencode_serve.rs` is the second worked example. It is a persistent server hosting sessions. A detached per-turn writer returns immediately. Structured capture reads the server's own message store rather than scraping a pane. An unattended permission posture is written into a generated config, because an unanswered approval prompt is a hang and a worker has no human.

## The alternative lane, and why it is a fallback

`codex exec resume <full-id> --json` chains one turn at a time from a given cwd. It works and it carries context. `cli/src/fno/agents/harnesses/codex.py` already spawns it. `harness_capabilities.toml` already declares the `headless_resume` form.

Its ceiling is structural, not a matter of polish.

- **No turn id.** Its `turn.started` event carries no identifier. So `turn/steer`'s precondition cannot be formed, and `turn/interrupt` has nothing to name. Steering and interruption are inexpressible, not merely unimplemented.
- **No `--sandbox` and no `--add-dir` on the resume path.** `-c/--config` is the only carrier. `cli/src/fno/agents/writable_dirs.py` builds this argv in both runtimes.
- **One process per turn.** A held connection pays boot once. This pays it every turn. Measured on one thread with matched prompts, per-turn overhead was roughly 10 to 11 seconds against 0.20 seconds.
- **No timestamps and no duration.** A driver cannot attribute latency. Items surface only on completion, so partial output is unobservable.

It stays the right shape for a one-shot. It is not a thread lane.

## What earns the bit

An unattended journey test, in the shape `opencode_serve.rs` established. Until it passes, `thread` stays `false` for codex.

1. Spawn a codex worker with `--substrate thread`. Assert the registry row carries `harness=codex`, a full `harness_session_id`, and `substrate=thread`. When either identity field is missing, refuse by name.
2. Drive one real turn that writes a file in the worker's own worktree, not the canonical checkout. Assert the file lands on the correct branch.
3. Kill the mux server. Assert the row survives and the thread is still resumable. This is the step that matters. Nothing in the tree performs it today.
4. Resume from a cold process. Assert a second turn quotes a fact introduced in the first. A recalled token is the positive marker. A non-empty reply is not.
5. Assert `review/start` on that thread returns a `reviewThreadId`. The worker is then reviewable and cannot fall through to self-attestation.
6. Assert that selection materialises a viewport inside a stated budget.

## Identity

`harness` plus the FULL `harness_session_id` bind a session, together. When a surface cannot produce both, it refuses by name and says which one is missing.

`attach_id` is never a binding key. A codex UUIDv7 head-8 is a roughly 65.5-second clock bucket. Siblings spawned in one minute collide. A join on it binds the wrong session for exactly the burst-spawned workers most likely to be present. Every RPC above names the full `threadId`. None accepts a short id.

A missing registry row is never evidence of death. Read the claim lockfile pid and probe that pid.
