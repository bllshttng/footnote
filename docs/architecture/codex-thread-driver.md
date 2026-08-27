# Codex thread driver: the measured protocol surface

Why codex has no thread lane today, what a driver must speak, and what earns the capability bit. Measured against `codex-cli 0.149.1` on 2026-08-26. The fleet table below carries the same reading for the other four harnesses.

A `thread` is an fno-layer construct. fno invents it and implements it over each harness's own resume primitive. No harness ships one. `bg` was only ever a claude subcommand. So this page does not ask whether codex has a thread feature. It answers what fno must build to own a codex thread lifecycle with no PTY.

Read this before you conclude that codex cannot have a thread lane. It can. Nothing has built one.

## The fleet, measured

The bit is per-harness, so the state of the other four is part of reading this page. Measured 2026-08-26 unless a cell says otherwise.

| harness | `thread` | durable store | resume primitive | lane state |
|---|---|---|---|---|
| claude | `true` | `~/.claude/jobs`, 82 sessions over 92 days | `claude --resume <uuid>` | shipped. A supervisor daemon forks detached children and reconnects from a roster at startup |
| opencode | `true` | session store behind `opencode serve` | `opencode --session <id>` | launch only. See the audit note below |
| codex | `false` | 2,745 rollouts back to 2025-10-16 | `codex resume <uuid>`, plus the 95-RPC app-server | none. The whole of this page |
| agy | `false` | 63 conversation DBs at `~/.gemini/antigravity-cli/conversations/<uuid>.db` | `agy --conversation <uuid>` | none |
| gemini | `false` | n/a | `gemini --resume <id>` | the CLI is deprecated upstream. agy is the successor |

Two readings follow, and neither is obvious from the table alone.

**agy has both durable halves, like codex.** Three turns ran on one conversation, each from a fresh process. Turns 2 and 3 both returned a token planted in turn 1. Per-turn wall clock was 7.64s, 4.56s and 4.67s. The `.db` filename equals the `conversation_id` the CLI returns in its own JSON, so the identity join needs no inference. `harness_capabilities.toml` records agy's `interactive_resume` as `kind = "unsupported"`, which understates the CLI. Correcting that row changes what fno will try to run, so it needs its own verification rather than a docs edit.

agy also holds a bidirectional NDJSON lane, and this was driven rather than inferred. Two turns went down one process that was never restarted. Turn 1 answered `ack` at step 17. Turn 2 answered the turn-1 token at step 19. The process was still alive afterwards.

```
agy --conversation <uuid> --input-format stream-json --output-format stream-json \
    --dangerously-skip-permissions -p=
```

`-p=` with an empty attached value is load-bearing. A bare `-p` swallows the next flag as its prompt, and `--print` alone is rejected with `flag needs an argument`. Input messages are `{"event":"user","message":{"role":"user","content":"<text>"}}`. A `content` array of `{"type":"text","text":...}` parts also parses. The vault documents the output stream only, so the event name came from the binary's own error strings.

That is the boot-once, drive-many property `codex exec resume` lacks, and it is the same shape as `provider.rs`'s `claude_stream_json_resume_argv`. So agy's lane can reuse machinery fno already ships for claude. Its `agent_response` steps also carry `text_delta` and a per-step `duration_seconds`, so a driver can attribute latency, which `codex exec resume` does not permit.

**opencode's `true` is the one bit not earned to this page's standard.** `harness_map.py`'s `substrate_default` returns `thread` for any harness whose bit is true. So opencode is the only harness routed onto a thread lane by default. Every harness with a false bit defaults to `headless`, a one-shot.

Meanwhile that lane has four gaps. It has no e2e spawn test through the real mux socket. It has no CI-installed binary behind its flag checks. It has no steering surface. Its liveness signal is the detached writer pid. That pid exits at the end of the first turn, while the session keeps working.

That last gap carries a reap hazard behind an opt-in setting rather than a live outage. It is a risk to close, not a fire.

The wider point is the one to take from this page. A single flag is covering two different claims. "Has a durable session lane" and "has a driver fno can steer" are not the same assertion. One flag standing for both is the shape AGENTS.md warns about under *never infer the axis from a value*.

## Three lanes, and which one you actually need

Before reading the protocol below, rule it out. Codex has three re-entry lanes and only one of them needs an RPC. A flag often does the job. Reaching past it for the protocol is the expensive mistake this section exists to prevent.

| lane | PTY | settings it carries | drives a turn with nobody watching |
|---|---|---|---|
| `codex resume <id> "<prompt>"` | required | `--cd`, `-s/--sandbox`, `--add-dir`, `-a/--ask-for-approval`, `-m/--model`, `--dangerously-bypass-hook-trust`, `-c` | no |
| `codex exec resume <id>` | no | `-c` only | yes, one turn per process |
| `codex app-server` | no | typed params on `thread/resume` | yes, many turns per process |

**`codex resume` is the right tool for materialise-on-selection.** It needs no driver. It takes the prompt as a positional argument, so it resumes and drives one turn in a single command. A row holding an id and a cwd survives any server restart on its own, because the rollout on disk is the durable object. The pane is disposable by design.

Two flags on that lane close the modals recorded as blocking unattended restore. `--cd <DIR>` sets the working root. Codex then never renders its "use session directory or current directory" prompt. That prompt defaults to the canonical checkout, and the correct answer is the worktree. `--dangerously-bypass-hook-trust` runs enabled hooks without persisted trust, which clears the hooks-trust gate. Neither needs a protocol and both were answered by hand three times on 2026-08-25.

**What fno's own resume lane applies is `--cd` alone.** Read the paragraph above as codex's capability, not as footnote's behavior. The gap is deliberate. A registry row records no sandbox posture. So the lane cannot tell a bounded worker from a yolo one, and a forced bypass resumes every bounded worker with approvals off. Hook trust needs that same operator choice, plus the installed-version gate the spawn lane already applies. So an unattended `fno agents resume` of a codex row with untrusted hooks still stalls on that prompt. Clearing it waits on an opt-in verb.

**What rules `codex resume` out is not capability, it is the terminal.** With stdin redirected it exits 1 in under a tenth of a second:

```
$ codex resume <id> --cd <dir> -s danger-full-access -a never "..." < /dev/null
Error: stdin is not a terminal
```

So a worker that must RECEIVE a turn while nobody is looking cannot use it. The only way into a running TUI is keystrokes at a pane. That path yields `typed (pane <id>)` rather than delivery. It also refuses a review verb outright while a task runs.

**That unattended case is the only one that needs more than a flag.** `codex exec resume` answers most of it. It needs no PTY and carries settings through `-c`. Its per-turn boot is roughly ten seconds, against a held connection's fraction of one. On turns that run for minutes that is a few percent, not a barrier.

What the protocol adds over `codex exec resume` is narrower than a feature list suggests. It is steering or interrupting a turn already in flight, which needs a turn id that `codex exec resume` never emits. And it is `review/start`, which returns a `reviewThreadId` and is the only route by which an unattended codex worker can be reviewed at all. That second one is an integrity property rather than a convenience, for the reason given under *A pane worker is not a hosted thread*.

Pick by the question being asked. A human coming back to a session wants `codex resume`. A worker taking one more turn wants `codex exec resume`. A worker that must be steered and reviewed unattended wants the protocol.

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
