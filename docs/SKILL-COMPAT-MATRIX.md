# footnote skill compatibility by driver

Which footnote skills work out of the box on each CLI driver, which need the loop wrapper, and which are not supported (yet).

## Legend

| Abbrev | Driver | Notes |
|---|---|---|
| `CC` | Claude Code | Native. Stop hook, Agent tool, usage-data capture. |
| `HER` | hermes-agent | Python CLI. `~/.hermes/skills/`, `delegate_task` tool, Nix-aware. |
| `OC` | openclaw | Node CLI. Typed plugin hooks, subprocess-spawn subagents. |
| `GEM` | Gemini CLI | Partial native support via `process` tool. |
| `CDX` | Codex CLI | Native plugin hooks, `CODEX_THREAD_ID`, project custom agents, and `spawn_agent`; explicit sequential fallback when a primitive is unavailable. |

| Status | Meaning |
|---|---|
| `OOTB` | Runs end-to-end once footnote skills are installed in the driver's skill path. No adapter needed. |
| `wrapper` | Works through `scripts/run-target-loop.sh --driver <name>`. Emits `<promise>` tag at iteration end; wrapper re-invokes until `MISSION COMPLETE` appears. |
| `partial` | Works for the single-turn path but loses some features (e.g., cached cost observability). |
| `-` | Not yet supported for this driver. |

## Classification method

Skills are classified by reading `SKILL.md` for three markers:

1. `<promise>` tag emission (definitive: loop-dependent)
2. `mode: autonomous` or fresh-context-per-iteration language
3. References to `target-state.md`, Stop hook, or external re-invocation

Skills with none of these run stateless and work OOTB on every driver that loads markdown skills.

## Matrix

| Skill | Classification | CC | HER | OC | GEM | CDX | Notes |
|---|---|---|---|---|---|---|---|
| audit | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Multi-perspective single-turn analysis. |
| blueprint | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Mutates a design doc into an execution plan; unsupported auto-launch primitives are reported before work starts. |
| cache-keepalive | CC-only | OOTB | - | - | - | - | Claude Code prompt-cache mechanism. Not applicable to other drivers. |
| check-pr | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | One GitHub poll per invocation. |
| codemap | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Pure Python + tree-sitter. |
| create-pr | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Mechanical `gh` invocation. |
| debug | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Hypothesis loop completes within one turn unless chained. |
| agent | orchestrator | OOTB | - | - | - | OOTB | Provider-native worker front door. Codex/Gemini build dispatch and prose handoff use provider-neutral briefs through autonomous spawn, return real receipts, and never send Claude slash commands; `bg` and `discuss` retain their documented Claude-only semantics. |
| distill | stateless | OOTB | partial | partial | partial | partial | Reads Claude Code observations; other drivers need skill-checkpoint parity. |
| do | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Lightweight single-session executor. Does not emit `<promise>`. |
| fix | hybrid | OOTB | wrapper | wrapper | wrapper | wrapper | Bounded iteration loop (N iterations). Wrapper restarts between iterations on non-CC. |
| git-worktrees | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Shell + native worktree API. |
| mail | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Runner-less front door over the `fno mail` durable mailbox (mirrors /agent): the model is the runner (a phone has no `!`). Verb router - `send`/`reply` (write; normalize.sh strips smart quotes, splits recipient/body, refuses an empty or whitespace-only recipient/body), `unread`/`list`/`view`/`status`/`ack`/`drain` (read/cursor, thin pass-through). Never confirms (messaging is free + async); reports the real msg-id receipt, never fabricated. Provider-neutral: needs only the `fno` binary (the mailbox is shared across claude/codex/gemini), so it runs OOTB on every driver - on a driver without `fno` it fails loud and writes nothing (degrade, never fake a delivery). |
| megawalk | loop | OOTB | wrapper | wrapper | wrapper | wrapper | Recursively invokes `/target`; wrapper must recurse. |
| megaspec | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Iterative spec refinement within single session. |
| operator | orchestrator | OOTB | OOTB | OOTB | OOTB | OOTB | Dispatches waves through the driver's subagent primitive. Codex uses project custom agents/`spawn_agent` when available and announces a sequential fallback otherwise. |
| target | loop | OOTB | wrapper | wrapper | wrapper | OOTB | Emits `<promise>MISSION COMPLETE</promise>`. Claude and Codex continue through native `Stop` hooks; wrapper drivers require external re-invocation. |
| target (plan-mode front door) | CC-only | OOTB | - | - | - | - | Native Plan Mode -> `/target` Mode 1. The capture hook fires on Claude Code's `ExitPlanMode` PostToolUse; Gemini/Codex have no such tool, so no sidecar is ever written and `/target` behaves exactly as today (no-op degradation). |
| target (bg-dispatch `bg`) | CC-only | OOTB | - | - | - | - | `/target bg` dispatches fresh `claude --bg` workers and remains Claude-only. Codex build dispatch is a separate prose-brief path through an owned-PTY `pane` or one-shot `headless` spawn; it never masquerades as `claude --bg`. |
| setup | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Interactive wizard; single conversation. |
| ship-docs | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Generates architecture + how-to docs. |
| sigma-review | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Spawns reviewer agents through the driver-native primitive. Codex uses project custom agents/`spawn_agent` when available and otherwise announces sequential execution. |
| spec | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Creates plan folder; no looping. |
| speculate | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | N worktree variants via the driver-native subagent primitive; Codex falls back explicitly to sequential execution when `spawn_agent` is unavailable. |
| tdd | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Red-green-refactor discipline, single session. |
| think | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Design exploration. |
| think-tank | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Multi-persona panel; single conversation. |
| token-doctor | CC-only | OOTB | - | - | - | - | Claude Code transcript + cache-metric parser. Not portable. |
| triage | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Graph ordering proposal; single turn. |
| what-if | stateless | OOTB | OOTB | OOTB | OOTB | OOTB | Scenario exploration. |

Row count: every entry under `skills/` at the time of writing.

## What each status means for the user

**OOTB** - install footnote skills once into the driver's skill path (e.g., `~/.hermes/skills/footnote`, openclaw's `local-loader` path, etc.); invoke the skill with the driver's usual `/skill-name` or `@skill-name` syntax. Nothing else required.

**wrapper** - install footnote skills as above, then invoke the skill through `scripts/run-target-loop.sh --driver <driver>`. The wrapper runs the bot as a subprocess, scans the output for `<promise>MISSION COMPLETE</promise>`, and re-invokes the bot with conversation history re-hydrated until the tag appears or the iteration cap is hit. See `docs/SETUP-HERMES.md` and `docs/SETUP-OPENCLAW.md` for per-driver install recipes.

**partial** - the skill's main code path works, but a feature specific to Claude Code (transcript format, usage-data capture, cache metric) is unavailable. The skill degrades gracefully.

**-** - not yet supported. Either the driver lacks a required primitive (cache metric, transcript export) or the integration is scheduled for a later spec.

## Why some skills are CC-only

A few skills hook into Claude Code internals that other drivers do not expose:

- `cache-keepalive` depends on Anthropic's prompt-cache semantics, which are meaningful only inside a Claude Code session where cache-read tokens are billable.
- `token-doctor` parses Claude Code's transcript JSONL format and cache-metric fields that hermes and openclaw transcripts do not produce.
- The `/target` plan-mode front door is gated on a PostToolUse hook matching Claude Code's `ExitPlanMode` tool. `EnterPlanMode`/`ExitPlanMode` are Claude Code constructs; Gemini and Codex have no equivalent, so the capture hook never fires and no `.fno/.pending-plan.md` sidecar is written. Detection in `/target` keeps the feature out of the portable driver-skill body (it lives in the optional hook + skill-relative scripts), so on those drivers `/target` runs unchanged.

These are marked `-` rather than `wrapper` because the wrapper does not help - the underlying feature has no driver-side equivalent.

## Where the wrapper hooks in

See [SETUP-HERMES.md](./SETUP-HERMES.md) and [SETUP-OPENCLAW.md](./SETUP-OPENCLAW.md) for install and first-run recipes.

The wrapper script itself lives at `scripts/run-target-loop.sh` and dispatches on `--driver`. Driver-specific functions (`driver_invoke`, `driver_check_promise`, `driver_persist_history`, `driver_default_max`) live in `scripts/lib/driver-claude-code.sh`, `scripts/lib/driver-hermes.sh`, and `scripts/lib/driver-openclaw.sh`.

## How to add a new driver

1. Implement `scripts/lib/driver-<name>.sh` with the four function contract.
2. Optionally add a bot-side sentinel plugin that writes `.fno/target-promise.signal` when a `<promise>` tag appears in the assistant response (see `docs/providers/promise-sentinel.md` for the protocol).
3. Add a row per skill to this matrix with the new column.
4. Write `docs/SETUP-<NAME>.md` with the install recipe.

The protocol is designed so that a new driver gets working coverage of every `OOTB` skill for free the moment footnote skills are installed in its skill path.
