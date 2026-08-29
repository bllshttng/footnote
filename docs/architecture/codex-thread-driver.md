# Codex thread driver: a client of the shared app-server daemon

What the codex thread lane speaks, which process owns a thread, and how a viewer reaches one. Measured against `codex-cli 0.149.1` on 2026-08-26, then again on 2026-08-28, after the driver moved onto the shared daemon. The fleet table below carries the same reading for the other four harnesses.

**One rule holds this page up. The viewport EXECS the harness's own attach command, and fno renders nothing.** Everything else here is detail under that.

A `thread` is an fno-layer construct. fno invents it and implements it over each harness's own resume primitive. No harness ships one. `bg` was only ever a claude subcommand. So this page does not ask whether codex has a thread feature. It answers which process owns a codex thread's lifecycle, and the answer is codex's own shared app-server daemon rather than anything fno runs.

Read this before you give fno that process to own. fno tried, and the cost is recorded under *Transport* below.

## The fleet, measured

The bit is per-harness, so the state of the other four is part of reading this page. Measured 2026-08-26 unless a cell says otherwise.

| harness | `thread` | durable store | resume primitive | lane state |
|---|---|---|---|---|
| claude | `true` | `~/.claude/jobs`, 82 sessions over 92 days | `claude --resume <uuid>` | shipped. A supervisor daemon forks detached children and reconnects from a roster at startup |
| opencode | `true` | session store behind `opencode serve` | `opencode --session <id>` | launch only. See the audit note below |
| codex | `true` | 2,745 rollouts back to 2025-10-16 | `codex resume <uuid>`, plus the 95-RPC app-server | shipped. A WebSocket client of the shared `codex app-server daemon`, which owns the thread |
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

**What fno's own resume lane applies is `--cd` alone.** Read the paragraph above as codex's capability, not as footnote's behavior. The gap was deliberate. A registry row recorded no sandbox posture. The lane had no way to tell a bounded worker from a yolo one, and a forced bypass resumed every bounded worker with approvals off. That much is fixed (registry v19): the codex thread lane stamps `sandbox_posture` at spawn and the daemon's `thread/resume` applies it. A `--yolo` worker keeps `danger-full-access` across a daemon restart. A pre-v19 row resumes with the safe `workspace-write` default. Hook trust still needs that same operator choice, plus the installed-version gate the spawn lane already applies. So an unattended `fno agents resume` of a codex row with untrusted hooks still stalls on that prompt. Clearing it waits on an opt-in verb.

**What rules `codex resume` out is not capability, it is the terminal.** With stdin redirected it exits 1 in under a tenth of a second:

```
$ codex resume <id> --cd <dir> -s danger-full-access -a never "..." < /dev/null
Error: stdin is not a terminal
```

So a worker that must RECEIVE a turn while nobody is looking cannot use it. The only way into a running TUI is keystrokes at a pane. That path yields `typed (pane <id>)` rather than delivery. It also refuses a review verb outright while a task runs.

**That unattended case is the only one that needs more than a flag.** `codex exec resume` answers most of it. It needs no PTY and carries settings through `-c`. Its per-turn boot is roughly ten seconds, against a held connection's fraction of one. On turns that run for minutes that is a few percent, not a barrier.

What the protocol adds over `codex exec resume` is narrower than a feature list suggests. It is steering or interrupting a turn already in flight, which needs a turn id that `codex exec resume` never emits. And it is `review/start`, which returns a `reviewThreadId` and is the only route by which an unattended codex worker can be reviewed at all. That second one is an integrity property rather than a convenience, for the reason given under *A pane worker is not a hosted thread*.

Pick by the question being asked. A human coming back to a session wants `codex resume`. A worker taking one more turn wants `codex exec resume`. A worker that must be steered and reviewed unattended wants the protocol.

## The bit is true, and what it now promises

`harness_capabilities.toml` holds `thread = true` for codex. The bit is evidence of a working driver, never an aspiration, and `harness_map.py`'s rule is that it is never inherited.

The claim it carries was measured wrong in both directions before the shared-daemon move. The bit was flipped to make a codex thread survive mux death, and a private app-server did survive that. It was a child of `fno-agents-daemon`, so it did not obviously survive THAT daemon's death, and the durability was narrower than advertised. Against the shared daemon the thread survives both, because neither process owns it.

That is now a promise the implementation keeps rather than one it overstates.

## The two durable halves already exist

Codex persists every conversation as a rollout at `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<full-session-id>.jsonl`. The full session id is in the filename. The working cwd is in the body. `codex resume <full-id>` re-enters one with context intact.

The transcript half and the resume half are both on disk. What is missing is an fno-side driver that owns the lifecycle.

## Transport: a WebSocket client of the shared daemon

`codex app-server` speaks JSON-RPC 2.0. The `"jsonrpc":"2.0"` header is omitted on the wire. There are two ways to reach one, and the choice of which decides far more than a dependency.

A private `codex app-server` child speaks newline-delimited JSON on stdin and stdout. The shared `codex app-server daemon` listens at `$CODEX_HOME/app-server-control/app-server-control.sock` and carries the same frames as WebSocket text, using the standard HTTP Upgrade handshake at `ws://localhost/rpc`. A bare NDJSON write to that socket, with no upgrade, closes the connection: transport, not protocol, is the whole difference between the two lanes.

**The driver takes the shared daemon, and the reason is not durability.** `crates/fno-agents/src/codex_thread.rs` calls `ensure_codex_daemon()`, connects through `codex_inject::connect_app_server`, and holds no child process. It used to fork a private app-server per worker as a child of `fno-agents-daemon`. On 2026-08-28 that left eight `codex app-server` processes parented to `fno-agents-daemon` beside one shared daemon holding the socket.

A private app-server owns no control socket. `codex agents`, `codex resume`, `codex fork`, `codex queue`, `codex archive` and Remote Control are all scoped to the shared one. `codex agents --help` says so in its own words: "Browse all agent sessions on the shared local app-server daemon." So a private child forfeits every vendor verb at once, including every verb the vendor ships next. The only symptom is that a thing the operator expects to work does not.

### Exec, never proxy

Viewing a thread is codex's DECLARED attach form, EXEC'd in a pane:

```
sh -c 'codex app-server daemon start; exec codex resume <session-id> --remote unix://'
```

The frames the driver reads drive turns. They never paint a screen. Read that as a hard boundary rather than a preference. A screenshot cannot tell an exec from a proxy, and the process tree can:

- No `codex app-server` has `fno-agents-daemon` as its parent. Read every hit's parent positively. Never count app-servers: a count of one is also what a broken daemon plus one orphan looks like.
- No `fno` process READS OR WRITES the bytes between a viewer terminal and `codex`. In the mux viewport the pane's own process is `codex`, with no children. The composed `exec` is load-bearing. It replaces the shell, so the pane's child is `codex` itself with no fno process in between. The `;` rather than `&&` is load-bearing too. A failed pre-exec still runs the attach. That yields the more specific error, in the pane the operator is already looking at.

A change that reads frames here to draw something has rebuilt the rendering layer this lane deleted, merely relocated into a pane. Both doors read ONE declaration now: the contract's `interactive_attach` row, overridable per harness. `fno` never links `fno-agents`. The test `attach_argv_matches_the_mux_renderer` links both crates and pins the two doors byte-identical for every harness the contract declares.

### The attach is a declaration, and an operator can correct it

`crates/fno-agents/src/harness_capabilities.toml` (and its byte-identical `cli/` copy) declares the argv:

```toml
[harness.codex.resume_strategy.forms.interactive_attach]
kind = "subcommand"
pre_exec = ["codex", "app-server", "daemon", "start"]
tokens = ["codex", "resume", "{session_id}", "--remote", "unix://"]
```

An operator can override it per harness without a release, in `.fno/config.toml`, project-local first, then global:

```toml
[harness.openclaw.attach]
tokens   = ["openclaw", "resume", "{session_id}"]
pre_exec = ["openclaw", "daemon", "start"]   # optional
```

`attach_form` in `crates/fno/src/agents_view.rs` is the ONE reader. Fail-open at every layer. An unreadable file, an unparseable file and an unparseable block each skip rather than clearing what was there. A typo cannot un-attach a working harness. An explicit `kind = "unsupported"` block is the one parsable way to retire a form. Config is read once per mux-server process. An edit takes effect at the next start.

Three facts a future change is likely to get wrong, all measured:

- **A bare `codex resume <id>` auto-attaches to a running daemon.** The marker: absent from `thread/loaded/list` before the resume, present after. Measured 2026-08-29 on 0.149.1. So `--remote unix://` is not what reaches the daemon. It is the assertion that the daemon is there. Against a daemon that is DOWN, the flag fails by name. A bare resume instead runs a private in-process app-server against a copy of the rollout. It hands back a session that looks correct.
- **`thread/resume` resolves a session BY its rollout, and `thread/start` writes none.** A turnless thread cannot be opened. That is why a seedless codex spawn takes one warmup turn (`WARMUP_SEED` in `daemon.rs`). It is also why a thread row with no session id on file refuses by naming the rollout. The vendor's bare "no rollout found for thread id" never passes through.
- **Detaching kills nothing.** The thread lives in the shared daemon. Closing the pane or exiting `codex resume` ends a viewer, not a worker.

### The daemon is ensured at every use, not once at spawn

The control socket FILE outlives the process that bound it, so an `exists()` probe reads healthy against a daemon dead for a day. Read the pid from `~/.codex/app-server-daemon/app-server.pid`, probe that pid, use its `processStartTime` as an incarnation token, and require the `initialize` handshake to return. `codex_inject::probe_codex_app_server` does exactly that.

The handshake answer is CHECKED, not just matched. A daemon that refuses `initialize` answers with an `error` frame on the matching id. That is the protocol-skew shape: codex offered 0.149.1 to 0.150.1 on the machine this was measured on. Treating the id match as success let a refusing daemon read healthy, never be rebooted, and fail every later call naming something else. It now reads unhealthy, so `ensure_codex_daemon` reboots it.

Spawn-time health does not survive to attach time either, and the attach no longer runs its own ensure: the declared `pre_exec` is the mechanism. When a daemon is already running, `codex app-server daemon start` is a documented no-op (measured: `{"status":"alreadyRunning"}` twice in a row), so the pair is cheap on every attach. A daemon that will not start surfaces codex's own error in the pane the operator is looking at. A driver connect still goes through `ensure_codex_daemon()` first.

**`codex app-server proxy --sock <path>` is unreliable.** It accepted frames and returned nothing against a confirmed-live daemon, twice, with no stderr. Do not build a transport on it. The WebSocket lane to that same socket answered on the first try.

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

## The sandbox carrier, measured rather than read

A thread worker needs the fno state root writable or it cannot claim a node, deliver mail, or spawn a peer. Every other codex lane grants that as argv. This lane builds no argv, so the grant has to be a protocol field, and which field is not a thing the docs settle.

Measured against the live app-server on 2026-08-28, codex-cli 0.149.1. The docs and the code disagreed going in, so the daemon's own answers are recorded here verbatim.

| Sent on `thread/start` | The daemon's answer |
|---|---|
| `"sandbox": "workspace-write"` | honored: resolves to `workspaceWrite` with `writableRoots: []` |
| `"sandbox": "workspaceWrite"` | REFUSED, `-32600 Invalid request: unknown variant workspaceWrite, expected one of read-only, workspace-write, danger-full-access` |
| `"sandbox": {object}` | refused, `-32600 invalid value: map, expected map with a single key` / `invalid type: map, expected unit` |
| `"sandboxPolicy": {type, writableRoots}` | ACCEPTED AND IGNORED: no error, and the thread resolves to the machine's configured default as though the field were absent |

So `thread/start` carries only the scalar enum. A `sandboxPolicy` there is the worst possible answer. It looks accepted and changes nothing. On a machine whose configured default is wider than `workspace-write`, it silently WIDENS the worker instead of narrowing it.

`turn/start` is the carrier. On a thread started bounded (`writableRoots: []`), a `turn/start` naming the state root let a shell command create a file under it. The identical turn with the root withheld was denied while still writing inside `cwd`. Both arms wrote a second file inside `cwd`, and both of those appeared. So the denied arm is a sandbox refusal, not a model that declined to run the command.

Two consequences worth stating, because both invert an obvious assumption:

The docs' `"workspaceWrite"` spelling is the one the server rejects, and `codex_thread.rs`'s `"workspace-write"` is the one it accepts. The code is right and the doc is wrong. A unit test pins the spelling so nobody "fixes" it toward the doc.

The roots are ADDITIVE, not a replacement. A bounded thread reports `writableRoots: []` and can still write its own `cwd`, so naming the state root does not take the worktree away.

The grant is sent on every turn rather than only the first. The protocol makes a turn-level override the thread's default for later turns. One turn is enough for a thread nobody resumes. A resumed thread re-resolves its posture server-side, so sending it per turn makes resume carry the grant for free.

The grant is per THREAD, never per daemon. One shared app-server owns every thread on the machine. A grant applied at daemon scope widens every other worker at once.

A live probe is the only instrument that can answer this. `codex_fake_daemon.rs` models no sandbox field at all. A green run against it measures the double and not the target. The fake records the frames it received instead, which pins the other half: fno's three hops put the roots on the wire.

## `thread/list` is a roster with the right cwd

One stdio child paged `thread/list` to exhaustion. It returned 630 threads. 171 of those sat across 121 distinct worktree paths, each carrying its true working directory.

This bears on a known hazard. `~/.fno/agents/registry.json` records the spawn dir, not the working dir. An auto-resume driven off it drops workers onto the default branch in the shared checkout. That failure looks like success, which is worse than a visible error.

Codex's own roster does not have the defect. The cwd is in the rollout body and `thread/list` surfaces it. A driver reading `thread/list` needs no cwd repair and no join to fix.

## A pane worker is not a hosted thread

This is the finding most likely to be re-derived the hard way.

An app-server hosts only the threads it started or resumed. A codex pane is a separate `codex` process with its own rollout. The app-server never had a handle on it. Measured with a confirmed-live daemon and roughly forty codex panes alive, `thread/loaded/list` returned zero.

The consequence is an integrity hazard, not a capability gap. `review/start` names a `threadId`. So it can never target a pane worker, however healthy the daemon is. The documented pane fallback is separately refused while a task runs. With both routes shut, an honest agent falls through to writing its own attestation and calling it review coverage.

A driver-hosted thread closes this by construction. It appears in `thread/loaded/list`, and `review/start` against it returns a receipt.

## What the fno side holds now

`crates/fno-agents/src/daemon.rs` is the supervisor, and `ctx.codex_threads` holds one entry per codex thread worker. Read those entries as SOCKETS, not children. The supervisor owns no app-server process, so losing one loses a connection while the thread keeps running. `recover_codex_threads` reopens them at startup from the registry's durable `harness_session_id`.

One actor task per thread owns its connection exclusively. That reasoning is about handle types, so it survived the transport change untouched. `drive_turn` used to hold a mutex guard for a whole turn, so every follow-up ask queued behind it and the steer RPC was unreachable. Consumers now send commands and never touch the driver.

Stopping a worker interrupts its in-flight turn and closes its connection. It does not kill an app-server. Killing that one takes every other codex session on the machine with it. A turn that survives the bounded settle keeps running on the daemon, and `stop` reports `timeout-turn-still-running` rather than claiming a kill it cannot perform.

## The alternative lane, and why it is a fallback

`codex exec resume <full-id> --json` chains one turn at a time from a given cwd. It works and it carries context. `cli/src/fno/agents/harnesses/codex.py` already spawns it. `harness_capabilities.toml` already declares the `headless_resume` form.

Its ceiling is structural, not a matter of polish.

- **No turn id.** Its `turn.started` event carries no identifier. So `turn/steer`'s precondition cannot be formed, and `turn/interrupt` has nothing to name. Steering and interruption are inexpressible, not merely unimplemented.
- **No `--sandbox` and no `--add-dir` on the resume path.** `-c/--config` is the only carrier. `cli/src/fno/agents/writable_dirs.py` builds this argv in both runtimes.
- **One process per turn.** A held connection pays boot once. This pays it every turn. Measured on one thread with matched prompts, per-turn overhead was roughly 10 to 11 seconds against 0.20 seconds.
- **No timestamps and no duration.** A driver cannot attribute latency. Items surface only on completion, so partial output is unobservable.

It stays the right shape for a one-shot. It is not a thread lane.

## The bar the bit is held to

The journey shape `opencode_serve.rs` established, kept here as the standing bar rather than a checklist that was ticked once.

1. Spawn a codex worker with `--substrate thread`. Assert the registry row carries `harness=codex`, a full `harness_session_id`, and `substrate=thread`. When either identity field is missing, refuse by name.
2. Drive one real turn that writes a file in the worker's own worktree, not the canonical checkout. Assert the file lands on the correct branch.
3. Kill the mux server. Assert the row survives and the thread is still resumable. Against the shared daemon this holds by construction, and so does killing `fno-agents-daemon`.
4. Resume from a cold process. Assert a second turn quotes a fact introduced in the first. A recalled token is the positive marker. A non-empty reply is not.
5. Assert `review/start` on that thread returns a `reviewThreadId`. The worker is then reviewable and cannot fall through to self-attestation.
6. Assert that selection materialises a viewport inside a stated budget.

**Assert a positive marker, never an absence.** The thread's id is PRESENT in `thread/loaded/list` read over a SEPARATE connection. Only a daemon that owns the thread answers that way, so the read cannot come back green for the wrong reason. Never count `codex app-server` processes. That count reads the same whether the daemon is healthy or dead with one orphan left behind. The no-child claim is pinned at the type level instead. An exhaustive match over `ThreadDriverError` carries no spawn arm, so it fails to COMPILE while the driver forks.

## Identity

`harness` plus the FULL `harness_session_id` bind a session, together. When a surface cannot produce both, it refuses by name and says which one is missing.

`attach_id` is never a binding key. A codex UUIDv7 head-8 is a roughly 65.5-second clock bucket. Siblings spawned in one minute collide. A join on it binds the wrong session for exactly the burst-spawned workers most likely to be present. Every RPC above names the full `threadId`. None accepts a short id.

A missing registry row is never evidence of death. Read the claim lockfile pid and probe that pid.
