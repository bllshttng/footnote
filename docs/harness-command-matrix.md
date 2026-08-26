# `fno agents`: every verb, per harness

`fno agents` supports five harness CLIs: `claude`, `codex`, `gemini`, `agy` (Antigravity), and `opencode`. Each harness has different substrates, session IDs, and re-entry paths. This page explains the contract. `cli/src/fno/agents/harness_capabilities.toml` is the authoring source. Cargo packages a byte-identical Rust copy. If the copies differ, tests fail.

Two runtimes serve the surface. The **Rust client** (`fno-agents`, the shipped default) intercepts most verbs; **Python** owns a handful (`whoami`, `top`, `peek`, `watch`, plus internal helpers) and is the fallback when no binary is installed (`FNO_AGENTS_RUNTIME=python`). Routing is automatic - you type `fno agents <verb>` either way. Notably, **pane spawns are Python-owned by design**: the mux-hosted back half lives in the Python `cmd_spawn` path, and the router keeps every pane spawn there even when Rust mode is requested - so the default substrate works identically under both runtimes.

Messaging note: `send` / `inbox` / `ack` are not direct lifecycle actions. They live under the `fno agents mail` subgroup.

## The harness model

What each harness fundamentally is, from fno's point of view:

| | claude | codex | gemini | agy | opencode |
|---|---|---|---|---|---|
| Substrates | pane, **thread**, headless | pane, headless | pane, headless | pane, headless | pane, **thread**, headless |
| Persistent-thread lane (`--substrate thread`) | yes (`claude --bg`) | no (hard error, use headless) | no | no | yes (a persistent session on a shared `opencode serve`, driven over HTTP) |
| Headless one-shot (`--substrate headless` / `--headless` / `-p` / `--once`) | yes (`claude -p`) | yes (`codex exec`) | yes (one-shot) | yes (`agy -p`) | yes (`opencode run`) |
| Session id recorded | `short_id` (jobId) + `harness_session_id` (full transcript UUID) | `harness_session_id` (full thread ID) | `harness_session_id` | **none** (stateless: plain-text output, no parseable ID) | `harness_session_id` (the `ses_` id, captured at spawn) |
| Re-enter a **live** session | `attach` / `resume` | `resume` | `resume` | no | `resume` |
| Revive a **dead** session | `spawn --resume <uuid>` (bg lane) | no | no | no | no |
| Read-only observation (`peek`, `logs`) | yes | yes | yes | yes | yes |

The pane substrate (the default) is the great equalizer: all five harnesses can be spawned as a mux-hosted interactive PTY pane. Everything asymmetric lives in the detached lanes.
Codex pane spawn waits for rollout binding for 60 seconds. A bound receipt includes `status: live`, `session_id`, and the derived eight-character `short_id`. If binding expires, fno reaps the pane and exits nonzero. It does not return an unaddressable `status: spawning` receipt.

agy pane spawns trust the exact cwd before launch. The shared gate clears remaining trust prompts. It submits seeds after the composer paints.

Known limitation: the agy seed receipt is unverified. Until action confirmation lands, `seed: submitted`, `readiness: live`, and `pane_observation: painted` do not prove that the worker consumed its seed. After spawning, run `fno agents peek <name>` and require a first worker action. If the exact pane stays idle, read its composer before you type anything. When the composer is EMPTY, recover once with `fno mux pane send <pane> --text '<prompt>' --raw --submit`, then peek again. When the composer already holds the seed, do NOT retype it, because a second `--text` write concatenates onto the buffer that is already there. The obvious remedy for that case does not work. Measured 2026-08-24 on a live agy pane, neither `--text '' --raw --submit` nor a bare carriage return submitted staged text. The cause is `submit_pane`, which sends one carriage return for every provider. Report a stranded composer rather than retyping into it. Do not re-seed a pane that is already working because that can queue a duplicate target.

## The opencode thread serve lane

`spawn --harness opencode --substrate thread` is the unattended opencode worker lane, and it is HTTP-driven rather than PTY-hosted. `bg` remains a deprecated input alias for one release:

- fno-agents manages one shared `opencode serve` per agents home (state file `opencode-serve.json`, health-gated reuse). The serve starts with a generated `OPENCODE_CONFIG`. That config grants `permission."*" = "allow"`, the unattended posture other worker lanes get from their own bypass flag. An unanswered permission `ask` on a headless server is a hang, not a refusal.
- The spawn mints a session with `POST /session?directory=<cwd>`, so one serve hosts workers across worktrees.
- The computed writable-dirs set rides `PATCH /session/:id`. The Python seam publishes it as `FNO_WORKER_ADD_DIRS` for every non-pane spawn. The set lands as per-session `external_directory` allow rules. This is the codex `--add-dir` pattern through opencode's native cell. It closes the claim-writes double-writer hazard.
- A detached `opencode run --attach <serve> --session <id>` writer drives the initial turn. Command-template expansion is native. The attach writer prints nothing on stdout. Structured capture is the serve's message readback over HTTP (role, model, parts). No pane scraping, no pipe scraping. The registry row's log path collects the writer's diagnostics.
- Registry identity: `harness_session_id` = the `ses_` id. Reachability is the existing store-membership probe (serve sessions share the global opencode store).
- Steering over the API is a filed follow-up: mail inject, ask, peek, resume. Until it lands, the `ask` verb still refuses with the pane-send pointer.

## Machine-readable interactive capabilities

Run `fno agents dispatch capabilities <h> --json` to read one harness without dispatch configuration. The JSON includes versioned data for permissions, sessions, readiness, input, stop, and removal. Missing or malformed fields stop contract loading. A harness never inherits Claude defaults.

| Harness | Permission response | Positive ready rule | Paste submission | Interactive resume | `stop` | `rm` harness cleanup |
|---|---|---|---|---|---|---|
| claude | `permission_prompt`: `1` once, `2` always, `3` deny | `live_prompt_box` | separate Enter after 800 ms | `claude attach <short_id>` | `claude stop <short_id>` | `claude rm <short_id>` <!-- retired-ok: a reference table of what each harness teardown invokes, not steps to run. --> |
| codex | `approval_prompt`: `1` once, `2` always, `3` deny | `idle_prompt` | **unsupported until a successful pane-submit fixture is pinned** | `codex resume <thread_id>`. Headless: `codex exec resume <thread_id>` | registry no-op | remove the thread from `session_index.jsonl` |
| gemini | unsupported | unsupported (deprecated lane) | unsupported | `gemini --resume <id>` | registry no-op | registry only |
| agy | unsupported | trust prompt cleared by submit | separate submit after readiness | unsupported on the interactive CLI | registry no-op | registry only |
| opencode | Known picker map: `Enter`, `Right Enter`, `Right Right Enter`. Automatic use requires a fingerprinted picker. | unsupported | unsupported | `opencode --session <ses_id>` | registry no-op | registry only |

`ready` means that the configured manifest rule matched. A painted pane with no positive rule stays `live`. Claude and Codex use different readiness rules.

Session binding has a separate type. Codex tries two oracles in order and waits up to 60 seconds total. First it reads a rollout-fd from the pane's own process tree. Codex 0.148 hands session ownership to a detached `codex app-server --remote-control` daemon that no longer exposes that fd there. So the second oracle reads the app-server's loaded threads for the pane's cwd instead. If neither oracle answers, fno reaps the pane and exits nonzero. `codex exec` (headless) still owns its own rollout and never needs the daemon oracle. The mux grab-work timeout is 75 seconds. A parity test requires 15 seconds more than the longest required binding window.

Claude uses a preassigned ID or SessionStart restamp. Gemini uses a preassigned ID. OpenCode uses a best-effort store lookup. Agy declares binding unsupported.

Permission responses are rule-gated. Without explicit authorization, a matched permission rule reports `blocked`, never `live`. With an authorized action, fno resolves the harness-native keys. It re-reads the prompt fingerprint while it holds the pane writer claim. Then it sends only those keys and waits for the positive ready marker.
## Verbs: creating and reviving workers

| Verb | claude | codex | gemini | agy | opencode | What it does |
|------|:---:|:---:|:---:|:---:|:---:|---|
| `spawn "<prompt>"` | yes | yes | yes | yes | yes | Create + register a worker. Default substrate `pane` (mux-hosted PTY). |
| `spawn --substrate thread` | yes | no | no | no | no | Persistent detached `claude --bg` thread. Hard error on any other harness, pointing to `headless`. `bg` is a deprecated alias for one release. |
| `spawn --substrate headless` / `--headless` / `-p` / `--once` | yes | yes | yes | yes | yes | One-shot: create + exchange + teardown. stdout is the harness reply. `-p` mirrors the harnesses' own one-shot short; `-H` is NOT a headless spelling, it selects the harness. OpenCode uses `opencode run`. |
| `spawn --harness <h>` / `-H <h>` | selector | selector | selector | selector | selector | Canonical CLI-binary selector (`claude\|codex\|gemini\|opencode\|agy`); the `--harness` vocabulary the rest of fno uses. A model VENDOR (`zai`, ...) is never a harness value; that is `--provider`/`-P`, a separate axis. Reassigned from headless: `-H` now takes a harness value, not a one-shot toggle. |
| `spawn --resume <uuid>` | yes (thread only) | no | no | no | no | **Revive a dead session**: mints a fresh detached thread seeded from the persisted transcript UUID, re-registers the row. Requires `--substrate thread` and harness claude. **Runtime caveat:** the `--resume` flag is wired only on the Python `cmd_spawn` path, so on an installed binary (default `auto`/`rust` runtime) the spawn auto-routes to the Rust client, which does not parse it; run it under `FNO_AGENTS_RUNTIME=python` until the flag joins the Python-only auto-route set. |
| `spawn --model <m>` | pane+thread+headless | pane+headless | pane+headless | pane+headless | pane | Exact passthrough to the harness CLI. Every harness honors it on pane; the one-shot lanes forward it too (`codex exec --model`, `gemini --model`, `agy`, `claude -p --model`). |
| `spawn --permission-mode <m>` | pane+thread+headless | pane | pane | pane | pane | Mapped approval mode (`claude -p`/`--bg` take it directly). Non-claude thread/headless lanes hardcode their own bypass form, so the flag is refused there (fail-closed, never silently dropped). Mutually exclusive with `--yolo`. |
| `spawn "<prompt>" -- <flags...>` | pane only (all five) | | | | | Provider-CLI passthrough: every token after `--` is spliced into the harness's own pane argv, so a flag fno never declared needs no code change. Refused on bg/headless (those builders carry none of the pane's guards). The tokens ride the composed argv through the same refusals that govern fno's own flags: `-p`/`--print` (claude, agy), `--settings`/`--session-id` (happy lane), codex's bare `exec` and opencode's bare `run` (headless subcommands); a token duplicating a flag fno already emitted (`--model`, `--permission-mode`, `--name`, or an alias of a permission flag fno emitted, e.g. gemini `--yolo` vs `--approval-mode`) is a named refusal, not a silent last-wins. The parser stays strict: a typo'd fno flag before `--` still fails locally instead of being forwarded. |

Retired creation verbs (each prints a pointer and exits non-zero, never a silent success): `host` and `promote` are gone - agent panes live in the mux now; use `fno agents spawn --name <n> --substrate pane`.

## Verbs: talking to and observing workers

| Verb | claude | codex | gemini | agy | opencode | What it does |
|------|:---:|:---:|:---:|:---:|:---:|---|
| `ask <name> <msg>` | id-bearing rows | id-bearing rows | id-bearing rows | no | no | Continue a registered session. A mux follow-up requires a pinned submit contract. Claude has one. Codex does not. Unsupported panes receive no bytes. |
| `fno agents mail send <name> "<text>"` | yes | daemon or durable | yes | mux pane | mux pane | Send asynchronously. Confirmed live delivery skips the bus. Unsupported or unconfirmed panes use the durable queue. |
| `watch <name>` | yes | no | no | no | no | Observe a held stream-json thread's turns in real time. claude-only transport. |
| `peek <name>` | yes | yes | mux scrollback or status | mux scrollback or status | mux scrollback or status | Read-only. Pane rows use mux scrollback. Paneless rows use transcript or normalized status events. |
| `attach <name>` | yes | no | no | no | no | Re-exec your terminal into the running session's own TUI (`claude attach <short_id>`). Requires the session to be **live**. |
| `resume <name> [--print-command] [--message/-m]` | yes (live only) | yes | yes | no | yes | Re-exec the harness's resume CLI in the agent's recorded cwd. A **live** claude row no longer exec's `claude attach <short_id>` directly; it wakes the session headlessly (pty-backed, route-settings-restored) and verifies it moved, since `claude attach` run non-interactively just prints "Attaching..." and exits. `--print-command` still prints the old `claude attach <short_id>` snippet for a human to run by hand. A **dead/exited** claude row is a genuine relaunch (`claude --resume <uuid>`), which still execs. Not the same door as `spawn --resume`. `--message`/`-m` is the text delivered on wake. |
| `logs <name>` | yes | yes | yes | yes | yes | Tail or follow the agent's log output (reads `log_path`). |

The three re-entry verbs are easy to conflate; the axes that separate them:

| | Session must be live? | Where you end up | ID it keys on |
|---|---|---|---|
| `attach` / `resume` (claude) | yes | your terminal, inside the session's TUI | `claude_short_id` (8-hex jobId) |
| `resume` (codex/gemini) | recorded session | your terminal, harness resume CLI | harness session ID |
| `spawn --resume <uuid>` | **no - it revives the dead** | a new detached bg worker + registry row (same conversation: `--resume` keeps the session UUID; only the supervisor and its jobId are new) | `claude_session_uuid` (full transcript UUID) |

## Verbs: registry and admin

Most verbs operate on the registry or daemon. When the capability contract declares a harness store, `stop` and `rm` use it.

| Verb | What it does |
|------|---|
| `list` | List registered agents with filters (includes discovered live claude sessions). |
| `status` | Daemon liveness + per-agent state (`status-v1.json`; warns on binary drift). |
| `whoami` | Print THIS worker's own registered mesh name + session ID. Run it when confused after compaction. |
| `top` | Every live worker process - fno-spawned and not - with RSS. |
| `trace <name>` | Trace an agent's dispatch lifecycle from `events.jsonl`. |
| `stop <name>` | Claude stops by short ID. Other harnesses return a successful registry no-op. This result does not prove process termination. Kill live mux panes with `fno mux pane kill`. |
| `rm <name>` | Refused while live. Claude removes by short ID, Codex removes the session-index entry, and Gemini/Agy/OpenCode remove only the fno registry row. |
| `reap [--json]` | Garbage-collect exited rows in bulk (same sweep as the daemon's idle tick; keeps rows whose worktree is dirty and tells you why). |
| `reconcile` | Sync registry status with harness reality; a live id-less Codex pane is bound from the rollout open in its own PID tree, while unresolved live panes remain `spawning` and dead panes retain orphan/exited behavior. |
| `restart` | Restart a stale daemon to pick up a new build; PTY workers survive. |
| `ping` | **Placeholder stub** - prints `(not yet implemented)` and exits 0 without probing anything. Do not script against it; use `status` for a real daemon probe. |

## Verbs: waiting and catch-up (harness-agnostic)

| Verb | What it does |
|------|---|
| `wait --agent <name> --state idle\|blocked\|done [--timeout-ms N]` | Block until the agent's registry row reaches the state. The scripting primitive. |
| `subscribe [--agent <name>] [--kinds state,exit]` | Stream registry state transitions + pane exits as they happen. |
| `digest --session <s> [--since <ts>]` | "While you were gone": fold events + ledger since a timestamp into a catch-up summary. |
| `needs [--since-epoch N]` | The needs-me queue: fold events + ledger across all projects into what wants operator attention. |

## Verbs: MCP channel sidecar (claude only)

There are no CLI verbs here any more.
`register-channel`, `unregister-channel`, and `push-channel` were removed: nothing shelled them, and `fno agents <verb>` now refuses each by name with a pointer (see `cli/src/fno/tombstones.py`).

The `channel.*` daemon RPCs they fronted are live and untouched.
The path in use is `fno.mcp.sidecar`, which speaks its own socket protocol (`register_channel` / `unregister_channel` ops) rather than going through the CLI.
The channel reaches only sessions launched with the channel wired; it is a claude-only transport this release.

## Verbs: loop and harness plumbing

You rarely type these by hand - hooks and drivers do - but they live under `fno agents`:

| Verb | Caller | What it does |
|------|---|---|
| `loop` | operator / dispatcher | Unified cross-session driver loop (`--driver target\|megawalk`). |
| `loop-check` | stop hook | The in-session stop/allow decision from external truth (PR, CI, review bots, budget). |
| `finalize` | loop-check terminal-allow | Idempotent ledger record + ship-time plan stamp. |
| `kill-check` | loop | Evaluate a plan's `kill_criteria`. |
| `verify-evidence` | gates | Verify child-promise event evidence and non-Claude agent presence. |
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
| `drive` | `fno mux pane send <pane> --raw ...`, or type into the pane in `fno mux`. `--raw` sends keystrokes verbatim; without it the payload is wrapped in an `<fno_mail>` envelope. `--text` fills the composer without submitting - see [Submitting a pane send](#submitting-a-pane-send). |
| `host` | `fno agents spawn --name <n> --substrate pane`. |
| `promote` | Same - the mux hosts agent panes now. |
| `send` / `inbox` / `ack` | The `fno agents mail` namespace (`fno agents mail send`, `fno agents mail inbox`, ...). |

Retired verbs print these pointers and exit non-zero, so scripts fail loud rather than silently succeeding.

## Submitting a pane send

`fno mux pane send <pane> --raw --text <s>` writes bytes into the composer. It does not submit. This default remains the staging contract for `fno mux block pipe`. Drop `--raw` only when the payload IS a peer message: the envelope is the default and keystrokes are the opt-out.

When delivery is intended, add `--submit`. The verb settles, sends a separate carriage return, and requires changed output. Unconfirmed text exits 22.

The internal delivery path uses this primitive. Agy seeds are submitted after trust and composer readiness. A carriage return inside pasted text is not Enter.

## Pointing the operator at a pane

Every other `mux pane` verb lets an agent act ON a pane.
`fno mux pane focus <pane>` is the one that moves the OPERATOR to one, so "it is in pane 31" becomes something their screen acts on.

```
$ fno mux pane focus 31
31 squad=2 (billing) tab=7 clients_moved=1
```

It resolves the pane session-wide.
A pane in another workspace or another tab needs no extra navigation.
Focusing a finished pane also clears its unseen marker, exactly as clicking it does.

Read `clients_moved`, not the exit code.
A zero-exit reply proves only that the command was accepted, never that anything moved on screen.
The count is how many attached viewers actually ended up looking at the pane.

Three refusals stay distinct, because "your pane is gone", "nobody is watching", and "your mux is not running" are different problems:

| Outcome | Exit | Message |
|---|---|---|
| No live pane with that id | 1 | `no such pane: <id>` |
| Running, but no non-passive client attached | 19 | `no attached client to move` |
| No reachable mux session | 1 | `cannot reach session <name>` |

There is deliberately no URL scheme for this.
`link.rs` `OPENABLE_SCHEMES` guards untrusted pane-sourced bytes.
Widening it to carry a deep link is a separate risk decision with its own blast radius.
The verb reaches the same destination with none of that exposure.

## Why the asymmetries exist

- **claude** is the only harness with a supervisor-managed detached thread (`claude --bg`), which is what makes the thread substrate, `attach`, `watch`, and dead-session revival (`spawn --resume` off the persisted transcript UUID) possible. When the supervisor dies, the short jobId dies with it - only the full session UUID survives on disk, which is why revival and attach key on different IDs.
- **codex / gemini** run as mux-hosted PTY panes (the Python back half) or through their own one-shot/resume CLIs. fno ships no thread lane for either, so neither has `attach`. For codex that is an unbuilt lane, not a ceiling. Its app-server drives a full no-PTY thread lifecycle over newline-delimited JSON on stdin/stdout: `thread/start`, `thread/resume`, `turn/start`, `turn/steer`, `turn/interrupt`. [codex-thread-driver](architecture/codex-thread-driver.md) records what a driver must speak and the journey test that earns the capability bit. The bit stays `false` until a driver ships. It is never inherited from protocol.
- A **codex** pane's full thread ID is the shared identity in the registry, mux `fno_id`, discovery handles, requested-name resolution, and any session-keyed node claim.
- Codex recovery uses that full thread ID as its only join.
- `fno agents watchdog --only recoverable --since 24h --cwd PATH` subtracts registered Codex rows from recent exact-cwd rollouts, reports both discovered and usable counts, and refuses a short ID or a rollout without readable transcript work.
- `--apply` restores only usable candidates as `origin=adopted` rows with full-UUID follow-up paths. It never spawns or resumes the thread. Usable candidates converge: the adopted row registers and the next scan subtracts it. An unusable rollout is refused and stays reported until it ages out of the `--since` window. The event lane names each refusal once per verdict change, not once per tick.
- A live recovery proof uses one disposable orphan and requires positive markers from the apply receipt, resumed reply, and exact full-ID row; row presence or an empty recovery queue is not proof:

```bash
repo=$(git rev-parse --show-toplevel)
sid="<full UUID from the disposable seed receipt>"
probe=$(uv run --project cli fno-py agents watchdog --only recoverable --since 24h --cwd "$repo" --session-id "$sid" --json)
jq -e '.complete == true and .recoverable_count == 1 and .usable_recoverable_count == 1' <<<"$probe"
apply=$(uv run --project cli fno-py agents watchdog --only recoverable --since 24h --cwd "$repo" --session-id "$sid" --apply --json)
jq -e --arg sid "$sid" '.results | length == 1 and .[0].session_id == $sid and .[0].outcome == "applied" and .[0].transcript_usable == true and .[0].registry_row_count == 1' <<<"$apply"
marker="RECOVERY-RESUME-OK-$(uuidgen | tr -d '-')"
reply=$(uv run --project cli fno-py agents ask "$sid" "Reply exactly $marker")
test "$reply" = "$marker"
uv run --project cli fno-py agents list --json | jq -e --arg sid "$sid" '.agents | any(.harness == "codex" and .harness_session_id == $sid and .last_message_at != null)'
```
- **agy** emits plain text with no parseable session ID, so it is **stateless**: the live pane works while attached, but there is nothing to re-enter after it settles. `ask`-by-name is refused; use a fresh `--once`.
- **opencode** is pane-hostable with a readiness detector and badge manifest. Its `ses_` session id is captured at spawn (a best-effort store lookup; an ambiguous or missed capture leaves the row live-only), probed for store membership, and resumable via `opencode --session <id>`. The fno plugin exposes the footnote verbs in opencode's command palette AND headlessly, so dispatch renders the native `/fno:verb` (not a prose brief). The headless spawn routes it through `opencode run --command fno:verb <args>` (a bare `run <message>` treats a leading slash as prose - verified against opencode v1.14.50), so a rendered slash command actually invokes the plugin command.

## Dispatch command surface

This table shows how autonomous dispatch renders a footnote `/verb` for each harness. `cli/src/fno/agents/harness_capabilities.toml` is the authoring source. `fno.agents.harness_map` loads it. Cargo packages a byte-identical Rust copy. `skills/agent/scripts/normalize.sh` mirrors the command-surface subset as a tested fallback.

| Harness | Rendered invocation | Notes |
|---|---|---|
| claude, agy | `/verb ...` | Native slash command (verbatim). |
| opencode | `/fno:verb ...` | Plugin-namespaced palette + `opencode run --command`. |
| codex | `$fno:verb ...` | `codex exec` expands the plugin skill. |
| gemini | **refused** | Deprecated; the dispatch lane is a loud error naming its successor (agy). No prose build brief is generated. |

Retask is a read-and-verified transaction over a live mux pane. It clears before switching. It waits for a changed harness session id. It renames the fno registry label. It verifies model and effort before submitting the no-merge target. Claude uses the declarative `direct` strategy. Codex uses `menu_walk` with live cursor reads. Gemini, agy, and opencode expose an unsupported strategy and retask refuses with `unsupported_switch_strategy` (spawn a fresh worker instead); only an axis mismatch returns `spawn_required`.

Only two spawn payloads render through this table: an **explicit `/verb` passthrough**, and a **resolved node-id build** (a node id -> `/target <id>`, the one surviving implicit `/target`, config-driven not shape-inferred). Any other free text is NOT wrapped - `spawn "<free text>"` sends it **verbatim as the session seed**, no `/target`, no per-harness render. To build free text, write `spawn /target <text>` or pass a node id. (The retired `ask`/`discuss` verbs are subsumed: a one-shot Q&A is the `headless` substrate; a conversational session is the default seed.)

## See also

- [provider-rotation.md](provider-rotation.md) - provider records, failover, and the switchboard settings schema.
- [harnesses/harness-adapters.md](harnesses/harness-adapters.md) - how a harness adapter is put together.
- `skills/using-fno/SKILL.md` - the two-surface orientation loaded each session.
