# The Target Pipeline

Target is the autonomous delivery loop. It takes an idea or a plan and ships a PR.

## How to use it

### From an idea

```
/fno:target "add user authentication with OAuth"
```

Target runs the full pipeline: think, plan, do, review, ship.

### From an existing plan

```
/fno:target path/to/plan/
```

Skips think and plan, goes straight to execution.

### Unattended, walk-away runs

Target's own loop is designed to keep going whether or not you're watching. For long overnight runs, set `config.target.restart_after_n_turns` (Phase 6 daemon) so the loop restarts with fresh context instead of degrading under compaction.

If you run many `/target` sessions across git worktrees of one repo and want the stop hook to clean up provably-dead duplicate state files, set `config.target.dedupe_dead_duplicates: true` (default `false`). When enabled, after the stop hook backfills a session's `claude_transcript_id`, a sibling worktree holding an `IN_PROGRESS` state with the same transcript id is renamed to `target-state.md.superseded` (recoverable) only when it is not the live worktree, its `owner_pid` is dead, and its state is older than the current one. The default-off resolution-time tiebreak already prevents false-orphans; this is an opt-in hygiene layer that narrows the duplicate set over time without ever touching a live sibling.

## What happens during a target run

```
think    Design exploration, ask questions, BDD acceptance criteria
  |
plan     Wave-based implementation plan with task breakdown
  |
do       TDD execution - write failing test, implement, verify
  |
review   6 parallel agents check code quality, UX, types, tests
  |
validate Build, lint, typecheck
  |
ship     Create PR, push to remote
  |
external Poll for Gemini/CodeRabbit review, address feedback
  |
docs     Generate architecture docs and how-to guides
```

Each phase updates `.fno/target-state.md` with progress. If the session crashes or compacts, target picks up where it left off.

## Size Profiles

Target uses t-shirt sizes to control ceremony level:

| Size | Flag | What it does |
|------|------|-------------|
| Small | `-S` | Uses /do executor. Build, review, PR. No docs, no external review, no verification. For bug fixes and quick features. |
| Medium | `-M` | Uses /do waves. Adds fresh verification, external review, docs. This is the default. |
| Large | `-L` | Uses /do waves with everything: research phase, adversarial challenge, browser testing, goal verification, clean pass, how-to guide. For critical production features. |

No flag means medium. Set a project default with `default_size: M` in `.fno/config.toml`.

### Overrides

Any individual flag works on top of a size:

```
/target -L --no-browser "feature"    # large without browser testing
/target -S --docs "quick fix"        # small but generate docs anyway
/target -M --adversarial "feature"   # medium plus adversarial challenge
```

### Legacy Flags

Old flags still work. `--lean` and `--quick` are aliases for `-S`. Individual skip flags (-D, -E, -G, -B, -H, -W) work as overrides on any size.

### Other Flags

| Flag | Effect |
|------|--------|
| `--cross-project` | Orchestrate across multiple repos |
| `--resume` | Continue from saved state |
| `--cancel` | Stop an active target session |
| `--max-iterations N` | Iteration cap |
| `--budget N` | Cost cap in USD |

## Quality gates

Target will not mark a run complete until ALL of these pass:

| Gate | Required | Can skip with | Notes |
|------|----------|---------------|-------|
| `quality_check_passed` | Always | - | Deferred: set only after all sigma-review agents return results and critical/high findings are addressed |
| `output_validated` | Always | - | External truth: CI green on the PR (`gh pr checks`); degraded fallback to the state boolean when gh/PR are unavailable |
| PR created | Always | - | |
| External review | Default yes | `--no-external` | Cron-based: two one-shot checks at +5 and +10 minutes |
| Goal verification | Opt-in | `--no-goals` (default) | |
| Browser testing | If has UI | `--no-browser` | |
| Docs generated | Default yes | `--no-docs` | |

If a gate fails, target fixes the issue and retries. After 3 identical failures, a circuit breaker trips and asks you what to do.

## The stop hook

The stop hook is what makes target autonomous. When target is running (status: IN_PROGRESS), the hook blocks session exit. The AI literally cannot quit until:

1. All quality gates pass
2. A `<promise>` tag confirms completion

**Session-scoped state preservation:** `init-target-state.sh` runs before the first tool call of every session. If it finds a `COMPLETE` or `BLOCKED` state file, it checks the `created_at` timestamp. State files newer than 300 seconds are left untouched - this prevents the hook from looping infinitely when a session completes normally within a short period. State files older than 300 seconds are treated as stale (left over from a previous session) and reset.

This is the core innovation. Other tools suggest code. Target ships it.

## Cross-project mode

```
/fno:target --cross-project "add auth to API and frontend"
```

This creates matching worktrees in each project, dispatches parallel subagents, and creates linked PRs. Projects execute in dependency order (backend first, frontend second).

Requires workspace configuration in `~/.fno/config.toml`. Run `/fno:setup` to configure.

## Resume after interruption

```
/fno:target --resume
```

Reads `.fno/target-state.md` and continues from the last completed phase.

## Common patterns

### Feature from idea
```
/fno:target "add drag-and-drop kanban board"
```

### Feature from plan (skip design phase)
```
/fno:target path/to/kanban-plan/
```

### Quick feature, skip ceremony
```
/fno:target --lean "add health check endpoint"
```

### Bug fix
```
/fno:fix investigate "users can't log in on Safari"
/fno:fix
```

## The Execution Hierarchy

footnote has five levels of execution, each wrapping the one below:

| Level | Skill | What it does | When to use |
|-------|-------|-------------|-------------|
| 5 | `/megawalk` | Vision to shipped product. Reads feature graph, picks ready features, dispatches target. | Multi-feature roadmap |
| 4 | `/target` | Idea to shipped PR. Won't quit until done. | Single feature |
| 3 | `/do waves` | Execute multi-phase plans with waves and verification. | Plan already exists |
| 2 | `/do` | Execute a focused plan in one shot. | Bug fix, small change |
| 1 | `archer` (agent) | Execute a single task with TDD. | Called by operator |

Each level composes the one below. Megawalk calls target. Target calls operator. Operator dispatches archer. You pick the level that matches your task.

Most of the time you'll use target (S/M/L). Megawalk is for when you have a vision document and want to ship multiple features continuously.

## Megawalk: The PM Layer

Megawalk is the PM. Target is the tech lead for each feature.

Megawalk reads from the feature graph (`~/.fno/graph.json`), which stores features with `ab-` prefixed IDs, `blocked_by` dependencies, and derived status. Key commands:

| Command | What it does |
|---------|-------------|
| `roadmap-tasks.py ready` | List features ready to execute |
| `roadmap-tasks.py tree` | Show dependency tree |
| `roadmap-tasks.py status` | Per-project progress with counts and cost |
| `roadmap-tasks.py next --claim {sid}` | Atomically pick and claim the next ready feature |
| `roadmap-tasks.py briefs --limit 5` | Load sidecar discovery briefs from completed features |

### Size Routing

Each feature in the graph has a `size` field (S/M/L) set during roadmap generation. If null, megawalk infers from task attributes:

- **Estimated points 1-3** or **1 phase plan**: `/target -S`
- **Estimated points 4-8** or **2-3 phase plan**: `/target -M`
- **Estimated points 9+** or **4+ phase plan**: `/target -L`

Domain modifiers adjust the size: infrastructure, security, and migration tasks go up one size, docs tasks go down one size.

You don't need to think about sizing when using megawalk. It handles it.

## Reliability features

Target runs two reliability passes that did not exist in the original loop. They reduce iterations spent on environmental issues and cross-phase context drift.

### Preflight

At the start of every `/target` run, the `target-preflight` step runs a fast environment audit (under 3 seconds, read-only). It checks: working tree clean, branch state, dependencies installed, codemap fresh, `gh` authenticated, disk space, and (opt-in) test suite green at HEAD.

If any check fails, target touches `.fno/.target-cancelled` and emits a `<promise>MISSION BLOCKED: preflight failure ...</promise>` so the stop hook writes `status: BLOCKED` cleanly. Total wall-clock cost on a fast-fail: a few seconds. Without preflight, the same run would have spent 15-20 minutes hitting the same problem at ship time.

To bypass preflight on a known-safe condition, pass `--skip-preflight`. The bypass is recorded in state for forensic review.

The full check catalog and contract are documented in `skills/target/references/preflight-checks.md`. Architecture deep-dive: `docs/architecture/target-reliability-core.md`.

### Phase handoff artifacts

Every target phase (`think`, `plan`, `do`, `clean`, `review`, `validate`, `ship`, `external`, `docs`) writes a small structured handoff artifact at `.fno/artifacts/handoff/{phase}-{session_id}.md`. The next phase reads its predecessor's artifact at start. This gives every transition a clean handoff without changing the state machine.

Each artifact is YAML frontmatter (per-phase schema) plus a short markdown summary, soft-capped at 500 tokens with a truncation marker for over-budget cases. They are additive context, not gates - a missing artifact never blocks target; the next phase just proceeds with reduced context and logs the gap.

Artifact paths are namespaced under `handoff/` to avoid collision with the gate-attestation artifacts that already live at `.fno/artifacts/{phase}-{session_id}.md` (the ones the stop hook reads for three-factor gate verification).

If you want to inspect what a phase passed to its successor, the helpers at `scripts/lib/phase-handoff.sh` (`ph_read`, `ph_read_latest`, `ph_list`) can be sourced directly from any shell.
