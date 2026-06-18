# Usage Detail

**Load when:** the user invokes target with no arguments (interactive mode wizard), OR you need the full execution-mode comparison, OR you're explaining context lifecycle (interactive vs unattended).

## Interactive Mode (no args)

When invoked with no arguments, use AskUserQuestion wizard:

**Input type:**
1. **Idea** — describe, `/blueprint` will write the plan
2. **Plan by path** — you have a plan file or folder ready
3. **Pick from backlog** — run `/triage` inline and propose the top-ranked pending node from `~/.fno/graph.json` with rationale. Requires the `triage` skill.

For option 3, run the triage flow (v2 preferred; the shim forwards through when `fno` is on PATH and falls back to the in-repo module otherwise):

```bash
(fno backlog triage context 2>/dev/null \
   || python3 scripts/triage.py context) > /tmp/triage-ctx.json
# Spawn triage subagent to produce proposal JSON, validate it, then pick
# the first ready node (no open blockers, highest priority) as the top
# recommendation.
```

Present the top-ranked node via AskUserQuestion: `Tackle ab-xxx "Title" - {rationale}? [Y/n]`. On yes, exec `/target {plan_path}` for that node. On no, fall through to option 2 (manual plan-by-path selection).

Then collect: execution mode (subagent/main thread), PR strategy (one/separate), optional skips (external review, docs). Skip wizard entirely if arguments provided.

## Execution Modes Table

| Mode | Subcommand | Git | Agents | Best For |
|------|------------|-----|--------|----------|
| **Main Thread** | (none) | Same branch | Sequential in main | Small features |
| **Subagent** | `agent` | Same branch | archer via Task (foreground) | Medium features, parallel waves |
| **Worktree** | `fork` | Separate branches | archer in worktrees (background) | Multi-plan, git isolation needed |
| **Cross-Project** | `cross-project` | Per-project branches | Parallel per project per step | Multi-repo features |

### Subagent Mode (`agent`)

Dispatches to archer agents via Task tool (foreground). Main thread coordinates, sees failures immediately. Parallel execution within waves, all on same branch.

### Worktree Mode (`fork`)

Same archer agents, but each runs in background in separate worktree. Git isolation per plan (own branch/PR). Tradeoff: target workers can get stuck.

See [multi-plan.md](multi-plan.md) for worktree details.

### Multi-Plan Mode (`fork`)

Creates **separate worktrees and atomic PRs** for each numbered plan file:
- Requires `fork` subcommand + folder path
- Only processes `[0-9][0-9]-*.md` files (excludes `00-*`)
- Each plan gets its own worktree, branch, and PR
- Outputs `<promise>` only when ALL plans complete

See [multi-plan.md](multi-plan.md) for the detailed protocol.

## Context Lifecycle Modes

This skill runs in **interactive mode** — the stop hook keeps the session alive. For autonomous (walk-away) execution, run target unattended via the external loop wrapper (`scripts/run-target-loop.sh`).

| | Interactive (`/target`) | Autonomous (unattended) |
|---|---|---|
| Session type | Interactive terminal | Print mode (`-p`) |
| Context management | Stop hook keeps alive | External loop restarts fresh |
| Compaction strategy | Context monitor warns user | Never compacts — exits before limit |
| Human intervention | User types /clear at breakpoints | None — loop handles everything |
| Exit signal | `<promise>` only | `<promise>` OR `<restart>` |
| Cost profile | Higher (compactions) | Lower (cold starts only) |
| Best for | Watching/monitoring work | Walk-away, overnight, agents |

## Override Flags

Negation flags stay as `--no-*` flags on top of any size. See [flag-migration.md](flag-migration.md) for the complete list.

Common overrides:

| Flag | Effect |
|------|--------|
| `--no-docs` | Skip docs (already off in S) |
| `--no-external` | Skip external review (already off in S) |
| `adversarial` (positional) | Add adversarial challenge (already on in L) |
| `combo <name>` (positional, 2 tokens) | Route via a provider combo (Plan B, ab-0e5a921e). Validates the combo via `fno providers combos list`; sets `TARGET_COMBO=<name>` in the env so spawned loop and target subprocesses inherit the routing. Resolution priority (highest first): per-agent pin > skill modifier > env > settings active_combo > active provider. |
| `--no-browser` | Skip browser testing (already off in S, M) |
| `--no-ship` | Skip PR creation (work stays local) |

## Model Optimization

Opus stays inline for phases 1-5 (think, plan, execute, review, validate) because:
- These need full conversation context and deep reasoning
- Cached input tokens (90% discount) make inline Opus cheaper than fresh agents
- 1M context window prevents freezing on long sessions

Ceremony phases (6-9) use cheaper models via spawned agents because:
- They don't need the parent's cached context
- Their tasks are self-contained with small, targeted input
- Fresh agents with 5-10K tokens of context work fine on Sonnet/Haiku
- Avoids the 200K freeze: keep agent context small, not the model choice

**When spawning ceremony agents, pass ONLY what the agent needs:**
- create-pr: branch name, commit log, PR template
- ship-docs: feature name, changed files, affected roles
- PR comment responses: review text, relevant code snippets

Do NOT pass full plan files or conversation history to ceremony agents.
