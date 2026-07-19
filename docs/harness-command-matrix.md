# `fno agents`: every verb, per provider

`fno agents` is one surface over five provider CLIs - `claude`, `codex`, `gemini`, `agy` (Antigravity), `opencode` - but the providers are not symmetric: substrates, session IDs, and re-entry paths differ per CLI. This page is the source of truth for what each verb does and which providers it works against.

Two runtimes serve the surface. The **Rust client** (`fno-agents`, the shipped default) intercepts most verbs; **Python** owns a handful (`whoami`, `top`, `peek`, `watch`, plus internal helpers) and is the fallback when no binary is installed (`FNO_AGENTS_RUNTIME=python`). Routing is automatic - you type `fno agents <verb>` either way. Notably, **pane spawns are Python-owned by design**: the mux-hosted back half lives in the Python `cmd_spawn` path, and the router keeps every pane spawn there even when Rust mode is requested - so the default substrate works identically under both runtimes.

Messaging note: `send` / `inbox` / `ack` are **not** `fno agents` verbs anymore - they moved to the dedicated `fno mail` namespace. The agents group is lifecycle-only.

## The provider model

What each provider fundamentally is, from fno's point of view:

| | claude | codex | gemini | agy | opencode |
|---|---|---|---|---|---|
| Substrates | pane, **bg**, headless | pane, headless | pane, headless | pane, headless | pane only |
| Detached-thread lane (`--substrate bg`) | yes (`claude --bg`) | no (hard error, use headless) | no | no | no |
| Headless one-shot (`--substrate headless` / `-H`) | yes (`claude -p`) | yes (`codex exec`) | yes (one-shot) | yes (`agy -p`) | **no** (refuses, pointing to pane) |
| Session id recorded | `claude_short_id` (jobId) + `claude_session_uuid` (full transcript UUID) | `codex_session_id` | `gemini_session_id` | **none** (stateless: plain-text output, no parseable ID) | `harness_session_id` (the `ses_` id, captured at spawn) |
| Re-enter a **live** session | `attach` / `resume` | `resume` | `resume` | no | `resume` |
| Revive a **dead** session | `spawn --resume <uuid>` (bg lane) | no | no | no | no |
| Read-only observation (`peek`, `logs`) | yes | yes | yes | yes | yes |

The pane substrate (the default) is the great equalizer: all five providers can be spawned as a mux-hosted interactive PTY pane. Everything asymmetric lives in the detached lanes.

## Verbs: creating and reviving workers

| Verb | claude | codex | gemini | agy | opencode | What it does |
|------|:---:|:---:|:---:|:---:|:---:|---|
| `spawn <name> [msg]` | yes | yes | yes | yes | yes | Create + register a worker. Default substrate `pane` (mux-hosted PTY). |
| `spawn --substrate bg` | yes | no | no | no | no | Persistent detached `claude --bg` thread. Hard error on any other provider, pointing to `headless`. |
| `spawn --substrate headless` / `-H` / `--once` | yes | yes | yes | yes | no | One-shot: create + exchange + teardown. stdout is the provider reply. |
| `spawn --resume <uuid>` | yes (bg only) | no | no | no | no | **Revive a dead session**: mints a fresh detached bg thread seeded from the persisted transcript UUID, re-registers the row. Requires `--substrate bg` and provider claude. **Runtime caveat:** the `--resume` flag is wired only on the Python `cmd_spawn` path, so on an installed binary (default `auto`/`rust` runtime) the spawn auto-routes to the Rust client, which does not parse it; run it under `FNO_AGENTS_RUNTIME=python` until the flag joins the Python-only auto-route set. |
| `spawn --model <m>` | pane+bg+headless | pane+headless | pane+headless | pane+headless | pane | Exact passthrough to the provider CLI. Every provider honors it on pane; the one-shot lanes forward it too (`codex exec --model`, `gemini --model`, `agy`, `claude -p --model`). |
| `spawn --permission-mode <m>` | pane+bg+headless | pane | pane | pane | pane | Mapped approval mode (`claude -p`/`--bg` take it directly). Non-claude bg/headless lanes hardcode their own bypass form, so the flag is refused there (fail-closed, never silently dropped). Mutually exclusive with `--yolo`. |

Retired creation verbs (each prints a pointer and exits non-zero, never a silent success): `host` and `promote` are gone - agent panes live in the mux now; use `fno agents spawn <name> --substrate pane`.

## Verbs: talking to and observing workers

| Verb | claude | codex | gemini | agy | opencode | What it does |
|------|:---:|:---:|:---:|:---:|:---:|---|
| `ask <name> <msg>` | id-bearing rows | id-bearing rows | id-bearing rows | no | no | Follow-up message to an already-registered agent (spawn creates; ask continues) - but only for rows carrying a recorded session id (bg/headless-created workers). A default **pane** worker registers with a mux ref and no resume id, so `ask` refuses it: type into its pane (`fno mux`) or use `fno mail send`. agy is stateless; opencode has no ask adapter. |
| `fno mail send <name> "<text>"` | yes | yes | yes | queues durable | queues durable | Async, durable-first delivery; never waits for a reply. Works on suspended/watch-only workers (the envelope is written before delivery is attempted; live delivery lanes exist for claude/codex/gemini). Registered-row required; unknown names exit 16. |
| `watch <name>` | yes | no | no | no | no | Observe a held stream-json thread's turns in real time. claude-only transport. |
| `peek <name>` | yes | yes | status events only | status events only | status events only | Read-only: recent transcript + status from disk. Never spawns anything, works on suspended and exited rows. The transcript-fallback arm supports claude and codex only; a gemini/agy/opencode row with no normalized status event exits 1 (`ObserveUnsupported`). The observe twin of `fno mail send`. |
| `attach <name>` | yes | no | no | no | no | Re-exec your terminal into the running session's own TUI (`claude attach <short_id>`). Requires the session to be **live**. |
| `resume <name> [--print-command]` | yes (live only) | yes | yes | no | yes | Re-exec the provider's resume CLI in the agent's recorded cwd. Note the claude arm builds `claude attach <short_id>` - on claude this is attach-with-cwd, not a dead-session revival (that is `spawn --resume`). `--print-command` prints the shell snippet instead of exec'ing. |
| `logs <name>` | yes | yes | yes | yes | yes | Tail or follow the agent's log output (reads `log_path`). |

The three re-entry verbs are easy to conflate; the axes that separate them:

| | Session must be live? | Where you end up | ID it keys on |
|---|---|---|---|
| `attach` / `resume` (claude) | yes | your terminal, inside the session's TUI | `claude_short_id` (8-hex jobId) |
| `resume` (codex/gemini) | recorded session | your terminal, provider resume CLI | provider session ID |
| `spawn --resume <uuid>` | **no - it revives the dead** | a new detached bg worker + registry row (same conversation: `--resume` keeps the session UUID; only the supervisor and its jobId are new) | `claude_session_uuid` (full transcript UUID) |

## Verbs: registry and admin (provider-agnostic)

These operate on the registry / daemon, not on a provider CLI, so they work for every provider's rows.

| Verb | What it does |
|------|---|
| `list` | List registered agents with filters (includes discovered live claude sessions). |
| `status` | Daemon liveness + per-agent state (`status-v1.json`; warns on binary drift). |
| `whoami` | Print THIS worker's own registered mesh name + session ID. Run it when confused after compaction. |
| `top` | Every live worker process - fno-spawned and not - with RSS. |
| `trace <name>` | Trace an agent's dispatch lifecycle from `events.jsonl`. |
| `stop <name>` | Stop the underlying session (idempotent; already-exited is a clean no-op). |
| `rm <name>` | Remove the registry row. Refused while live - stop first. |
| `reap [--json]` | Garbage-collect exited rows in bulk (same sweep as the daemon's idle tick; keeps rows whose worktree is dirty and tells you why). |
| `reconcile` | Sync registry status with provider reality. |
| `restart` | Restart a stale daemon to pick up a new build; PTY workers survive. |
| `ping` | **Placeholder stub** - prints `(not yet implemented)` and exits 0 without probing anything. Do not script against it; use `status` for a real daemon probe. |

## Verbs: waiting and catch-up (provider-agnostic)

| Verb | What it does |
|------|---|
| `wait --agent <name> --state idle\|blocked\|done [--timeout-ms N]` | Block until the agent's registry row reaches the state. The scripting primitive. |
| `subscribe [--agent <name>] [--kinds state,exit]` | Stream registry state transitions + pane exits as they happen. |
| `digest --session <s> [--since <ts>]` | "While you were gone": fold events + ledger since a timestamp into a catch-up summary. |
| `needs [--since-epoch N]` | The needs-me queue: fold events + ledger across all projects into what wants operator attention. |

## Verbs: MCP channel sidecar (claude only)

| Verb | What it does |
|------|---|
| `register-channel` / `unregister-channel` | Register/unregister a Claude Code session as an agent channel. |
| `push-channel` | Push a message to a registered channel. |

The channel reaches only sessions launched with the channel wired; it is a claude-only transport this release.

## Verbs: loop and harness plumbing

You rarely type these by hand - hooks and drivers do - but they live under `fno agents`:

| Verb | Caller | What it does |
|------|---|---|
| `loop` | operator / dispatcher | Unified cross-session driver loop (`--driver target\|megawalk`). |
| `loop-check` | stop hook | The in-session stop/allow decision from external truth (PR, CI, review bots, budget). |
| `finalize` | loop-check terminal-allow | Idempotent ledger record + ship-time plan stamp. |
| `kill-check` | loop | Evaluate a plan's `kill_criteria`. |
| `verify-evidence` | gates | Verify subagent/child-promise event evidence. |
| `report` | any harness's hooks | Inside-leg state push (working/blocked/done + reason) that powers the sideline badges. |
| `spawn-guard` | dispatch scripts | Shared bg-dispatch claim guard (node-claim probe + dispatch reservation). |
| `drive-authority` | mux/daemon | Drive-authority arbitration for owned panes. |
| `discovered-json` | Rust `list` | Internal: emits discovered live claude sessions for the `list` render path. |
| `nudge-peek` | Rust `loop-check` | Internal: loop-boundary inbox nudge read. |
| `gate` | (retired) | Prints a retirement pointer - the injection gate died with daemon PTY hosting at G4. |

## Retired and relocated verbs

| Old verb | Where it went |
|---|---|
| `grid` | The mux. Open `fno mux`; script panes with `fno mux pane ls\|read\|run\|send\|wait\|kill`. |
| `drive` | `fno mux pane send <pane> ...`, or type into the pane in `fno mux`. |
| `host` | `fno agents spawn <name> --substrate pane`. |
| `promote` | Same - the mux hosts agent panes now. |
| `send` / `inbox` / `ack` | The `fno mail` namespace (`fno mail send`, `fno mail inbox`, ...). |

Retired verbs print these pointers and exit non-zero, so scripts fail loud rather than silently succeeding.

## Why the asymmetries exist

- **claude** is the only provider with a supervisor-managed detached thread (`claude --bg`), which is what makes the bg substrate, `attach`, `watch`, and dead-session revival (`spawn --resume` off the persisted transcript UUID) possible. When the supervisor dies, the short jobId dies with it - only the full session UUID survives on disk, which is why revival and attach key on different IDs.
- **codex / gemini** run as mux-hosted PTY panes (the Python back half) or through their own one-shot/resume CLIs. No detached thread means no bg lane and no attach.
- **agy** emits plain text with no parseable session ID, so it is **stateless**: the live pane works while attached, but there is nothing to re-enter after it settles. `ask`-by-name is refused; use a fresh `--once`.
- **opencode** is pane-hostable with a readiness detector and badge manifest. Its `ses_` session id is captured at spawn (a best-effort store lookup; an ambiguous or missed capture leaves the row live-only), probed for store membership, and resumable via `opencode --session <id>`. The fno plugin exposes the footnote verbs in opencode's command palette AND headlessly, so dispatch renders the native `/fno:verb` (not a prose brief). The headless spawn routes it through `opencode run --command fno:verb <args>` (a bare `run <message>` treats a leading slash as prose - verified against opencode v1.14.50), so a rendered slash command actually invokes the plugin command.

## Dispatch command surface

How an autonomous/`/agent spawn` dispatch of a footnote `/verb` is rendered per harness. The single source of truth is `fno.agents.harness_map` (`fno dispatch resolve`); `skills/agent/scripts/normalize.sh` mirrors it as a static fallback (a test asserts parity).

| Harness | Rendered invocation | Notes |
|---|---|---|
| claude, agy | `/verb ...` | Native slash command (verbatim). |
| opencode | `/fno:verb ...` | Plugin-namespaced palette + `opencode run --command`. |
| codex | `$fno:verb ...` | `codex exec` expands the plugin skill. |
| gemini | **refused** | Deprecated; the dispatch lane is a loud error naming its successor (agy). No prose build brief is generated. |

Only two spawn payloads render through this table: an **explicit `/verb` passthrough**, and a **resolved node-id build** (a node id -> `/target <id>`, the one surviving implicit `/target`, config-driven not shape-inferred). Any other free text is NOT wrapped - `spawn "<free text>"` sends it **verbatim as the session seed**, no `/target`, no per-harness render. To build free text, write `spawn /target <text>` or pass a node id. (The retired `ask`/`discuss` verbs are subsumed: a one-shot Q&A is the `headless` substrate; a conversational session is the default seed.)

## See also

- [provider-rotation.md](provider-rotation.md) - provider records, failover, and the switchboard settings schema.
- [providers/provider-adapters.md](providers/provider-adapters.md) - how a provider adapter is put together.
- `skills/using-fno/SKILL.md` - the two-surface orientation loaded each session.
