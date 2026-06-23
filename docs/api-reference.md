# footnote Plugin - API Reference

The **footnote** plugin is an autonomous development workflow for Claude Code that takes features from idea to shipped PR. It provides 26 skills, 12 agents, and a hook system that work together as a pipeline: think, spec, do, review, ship.

- **Repository**: `footnote/`
- **Plugin manifest**: `.claude-plugin/plugin.json`
- **Author**: Jason Noah Choi
- **Version**: 1.0.0

---

## Table of Contents

- [Skills API Reference](#skills-api-reference)
  - [Workflow Skills](#workflow-skills)
  - [Design Skills](#design-skills)
  - [Execution Skills](#execution-skills)
  - [Review Skills](#review-skills)
  - [Utility Skills](#utility-skills)
- [Agent Contracts](#agent-contracts)
  - [Execution Agents](#execution-agents)
  - [Review Agents](#review-agents)
  - [Verification Agents](#verification-agents)
  - [Specialized Agents](#specialized-agents)
- [Hook Events](#hook-events)
  - [Hook Config](#hook-config)
  - [Platform Variants](#platform-variants)
- [State File Schemas](#state-file-schemas)
- [Configuration](#configuration)
- [Installation](#installation)

---

## Skills API Reference

Skills live in `skills/<name>/` and each contains a `SKILL.md` with YAML frontmatter and markdown instructions. Optional `references/` and `scripts/` subdirectories provide supporting material.

### Workflow Skills

The core pipeline - from idea to shipped PR.

| Skill | Command | Model | Purpose |
|-------|---------|-------|---------|
| `think` | `/fno:think` | inherit | Brainstorming and design exploration with BDD criteria |
| `blueprint` | `/fno:blueprint "feature"` | inherit | Implementation planning with wave execution strategy. Use `--full` for BDD acceptance criteria |
| `do` | `/fno:do` | inherit | Lightweight single-session plan execution. Supports `--resume` and `--retry <task-id>` |
| `operator` | `/fno:operator` | inherit | Heavy multi-phase wave orchestration with subagent dispatch |
| `target` | `/fno:target "feature"` | inherit | Autonomous end-to-end pipeline (think, blueprint, do, review, ship). Also accepts a plan path to skip think/blueprint |
| `megawalk` | `/fno:megawalk` | inherit | Multi-session task orchestration from a vision document |

#### target Pipeline Stages

```
Input ("feature" or path/to/plan)
  -> think (design exploration)
  -> plan (wave strategy)
  -> do (execute waves, spawn subagents)
  -> sigma-review (6 parallel review agents)
  -> goal-verification (3-level check)
  -> create-pr (fork to Haiku)
  -> <promise> tag signals completion
```

#### do / operator Differences

| Aspect | `/fno:do` | `/fno:operator` |
|--------|------------------|--------------------|
| Weight | Lightweight, in-session | Heavy, multi-agent |
| Orchestration | Reads 00-INDEX.md, runs waves inline | Spawns subagent per task via orchestrator.py |
| Resume | `--resume`, `--retry <id>` | STATE.md based |
| Best for | Small-to-medium plans | Large plans with parallel waves |

### Design Skills

Pre-implementation analysis and documentation.

| Skill | Command | Purpose |
|-------|---------|---------|
| `audit` | `/fno:audit` | Multi-perspective completeness analysis of a feature or plan |
| `what-if` | `/fno:what-if` | Scenario exploration, edge cases, and failure mode analysis |

### Execution Skills

Building, testing, fixing, and verifying code.

| Skill | Command | Purpose |
|-------|---------|---------|
| `tdd` | `/fno:tdd` | Test-driven development enforcement (red-green-refactor) |
| `fix` | `/fno:fix` | Autonomous fix loop - max 15 iterations, auto-reverts on regression |
| `debug` | `/fno:debug` | Scientific method bug hunting with hypothesis loop |
| `speculate` | `/fno:speculate` | Run N parallel variations of an approach and pick the best |

#### fix Loop Protocol

```
1. Identify the failure (test, lint, runtime)
2. Form ONE hypothesis
3. Apply ONE fix
4. Run verification
5. If regression detected -> auto-revert
6. Repeat (max 15 iterations)
```

#### debug Hypothesis Loop

```
1. Reproduce the bug
2. Form hypothesis
3. Gather evidence (logs, traces, state inspection)
4. Confirm or reject hypothesis
5. Track attempt to avoid repeating failed approaches
6. If rejected -> next hypothesis
7. If confirmed -> apply fix -> verify
```

### Review Skills

Code quality, testing, and PR management.

| Skill | Command | Purpose |
|-------|---------|---------|
| `sigma-review` | `/fno:sigma-review` | Orchestrates 6 parallel review agents on changes |
| `check-pr` | `/fno:check-pr` | Polls for external reviewer feedback and implements changes |
| `create-pr` | `/fno:create-pr` | Creates PR with description. Runs in fork context on Haiku |

#### sigma-review Agent Suite

When `/fno:sigma-review` runs, it dispatches 6 specialized review agents in parallel:

| Agent | Focus Area |
|-------|------------|
| `code-reviewer` | CLAUDE.md compliance, bug detection |
| `silent-failure-hunter` | Swallowed errors, inadequate error handling |
| `type-design-analyzer` | Type invariants, encapsulation |
| `integration-test-analyzer` | Journey tests, DB verification |
| `ux-flow-tester` | User journeys, error states |
| `multi-device-checker` | Responsive design, touch targets |

### Utility Skills

Configuration, analysis, and operational tools.

| Skill | Command | Purpose |
|-------|---------|---------|
| `setup` | `/fno:setup` | Interactive settings.yaml configuration wizard |
| `codemap` | `/fno:codemap` | AST-based structural analysis with PageRank (god nodes, orphans, module boundaries) |
| `ship-docs` | `/fno:ship-docs` | Architecture documentation generation |
| `git-worktrees` | `/fno:git-worktrees` | Git worktree creation and management |

#### Multi-repo features

There is no cross-project orchestration skill. A multi-repo feature is decomposed into one backlog node per project (linked by `blocked_by`); each ships its own PR from its own repo, and spawn-into-project dispatches the cross-repo handoff (`/do` auto-spawns foreign unblocked waves; `fno backlog advance` dispatches dependents on merge).

---

## Agent Contracts

Agents live in `agents/<name>.md` with YAML frontmatter defining `name`, `description`, `model`, `color`, `tools`, and optional `skills`.

### Shared Return Contract

All execution agents return a structured result:

```
RESULT: SUCCESS | DONE_WITH_CONCERNS | FAILED | BLOCKED
TASK: <task-id>
COMMIT: <hash>                    # if SUCCESS or DONE_WITH_CONCERNS
CONCERNS: <description>           # if DONE_WITH_CONCERNS
ERROR: <message>                  # if FAILED
REASON: <description>             # if BLOCKED
UNBLOCKS_AFTER: <prerequisite>    # if BLOCKED
```

### Execution Agents

| Agent | Model | Color | Tools | Purpose |
|-------|-------|-------|-------|---------|
| `archer` | sonnet | cyan | Read, Write, Edit, Grep, Glob, Bash | TDD task executor. Implements tasks with test-first methodology |

**Skills loaded**: `tdd`

**Disallowed tools**: Task, WebSearch, WebFetch, NotebookEdit

#### archer Startup Protocol

1. Read `.fno/current-PLAN.md` for task specification
2. Read `.fno/CONTEXT.md` for user constraints (if exists)
3. Read `.fno/STATE.md` for prior progress (if exists)
4. Run pre-flight checks (test runner, environment deps)
5. If pre-flight fails - return BLOCKED immediately

#### target TDD Discipline

1. Write failing test
2. Verify it fails (red)
3. Implement minimal code (green)
4. Verify database state (not just UI)
5. Atomic commit (one task = one commit)

### Review Agents

| Agent | Model | Color | Focus |
|-------|-------|-------|-------|
| `code-reviewer` | opus | green | CLAUDE.md compliance and bug detection. Uses confidence scoring (0-100). Only reports issues at 80+ confidence |
| `silent-failure-hunter` | inherit | red | Swallowed errors, empty catch blocks, inadequate error handling |
| `type-design-analyzer` | sonnet | magenta | Type invariants, encapsulation, type system misuse |
| `integration-test-analyzer` | inherit | green | Journey tests, database verification, integration coverage |
| `ux-flow-tester` | sonnet | cyan | User journeys, error states, edge case UX |
| `multi-device-checker` | sonnet | blue | Responsive design, touch targets, viewport handling |

#### code-reviewer Confidence Thresholds

| Range | Meaning |
|-------|---------|
| 0-25 | Likely false positive or pre-existing issue |
| 26-50 | Minor nitpick not in CLAUDE.md |
| 51-75 | Valid but low-impact |
| 76-90 | Important - requires attention |
| 91-100 | Critical bug or explicit CLAUDE.md violation |

### Verification Agents

| Agent | Model | Color | Tools | Purpose |
|-------|-------|-------|-------|---------|
| `verifier` | haiku | yellow | Read, Grep, Glob, Bash | 3-level verification: exists, substantive, wired. Checks deliverables against PLAN.md criteria |
| `goal-verifier` | sonnet | orange | Read, Grep, Glob, Bash | Original goal achievement validation. Detects phantom completion - never trusts SUMMARY.md claims |

#### 3-Level Verification Protocol

1. **Exists** - Does the artifact exist in the codebase?
2. **Substantive** - Is it a real implementation (not a stub or placeholder)?
3. **Wired** - Is it connected to the rest of the system (imported, routed, called)?

### Specialized Agents

| Agent | Model | Color | Tools | Purpose |
|-------|-------|-------|-------|---------|
| `tournament-debugger` | sonnet | red | Read, Grep, Glob, Bash | Parallel hypothesis testing - multiple agents compete to find root cause |
| `roadmap-generator` | opus | green | Read, Write, Edit, Bash, Grep, Glob | Generates prioritized task backlog from vision documents for megawalk |
---

## Commands

Slash commands in `commands/` (1 total). These are user-facing entry points that invoke skills.

| Command | File | Purpose |
|---------|------|---------|
| `/fno:cancel-target` | `cancel-target.md` | Stop active target loop (removes state file) |

---

## Hook Events

Hooks provide lifecycle control for autonomous loops, context injection, and signal capture. Configuration lives in `hooks/`.

### Hook Config

**File**: `hooks/hooks.json`

```json
{
  "description": "footnote plugin hooks for target loops, context monitoring, ...",
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/hooks/target-stop-hook.sh"
          },
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/hooks/convo-signal-capture.sh"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "node ${CLAUDE_PLUGIN_ROOT}/hooks/context-monitor.js"
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PLUGIN_ROOT}/hooks/inject-project-vision.sh"
          }
        ]
      }
    ]
  }
}
```

### Event Types

| Event | Trigger | Hooks |
|-------|---------|-------|
| `Stop` | Session exit attempted | `target-stop-hook.sh` - blocks exit when `target-state.md` shows `status: IN_PROGRESS` and output lacks a `<promise>` tag. `convo-signal-capture.sh` - captures conversation signals |
| `PostToolUse` | After any tool call | `context-monitor.js` - monitors context usage and token budget |
| `SessionStart` | Session begins | `inject-project-vision.sh` - loads project vision into context |

### Hook Scripts

| Script | Purpose |
|--------|---------|
| `target-stop-hook.sh` | In-session loop control. Prevents premature exit during autonomous pipeline |
| `convo-signal-capture.sh` | Captures conversation signals for telemetry |
| `context-monitor.js` | Token budget monitoring and context management |
| `inject-project-vision.sh` | Injects project vision document at session start |
| `session-start.sh` | General session bootstrap |

### Platform Variants

| File | CLI | Notes |
|------|-----|-------|
| `hooks.json` | Claude Code | Native hook format using `${CLAUDE_PLUGIN_ROOT}` |
| `hooks-gemini.json` | Gemini CLI | Adapts to Gemini lifecycle events |
| `hooks-codex.json` | Codex CLI | Adapts to Codex hook system |

### Looping Mechanisms

#### In-Session Loop (Stop Hook)

The stop hook (`target-stop-hook.sh`) blocks session exit when:
- `target-state.md` has `status: IN_PROGRESS`
- The model's output does **not** contain a `<promise>` tag

This keeps the autonomous pipeline running until completion.

#### Cross-Session Loop (External)

`scripts/run-target-loop.sh` re-invokes the CLI until:
- A `<promise>` tag appears in output, OR
- Max iterations reached

#### Promise Tag

Autonomous loops signal completion with:

```xml
<promise>MISSION COMPLETE: all tasks done, tests passing, review feedback addressed, PR created</promise>
```

---

## State File Schemas

State files live in `.fno/` at the project root. They are created and managed by skills and agents during execution.

### target-state.md

Immutable session manifest, written once by `fno target init`. Owned by the target skill. After the control-plane collapse this is an inputs-only file: no `status`, no `current_phase`, no gate booleans, no `iteration`. It records the session's inputs and is never mutated post-init (the sole exception is a first-fill of an empty `plan_path` via `fno state set`).

```yaml
---
session_id: "..."
created_at: 2026-06-07T12:00:00Z
input: "feature description or plan path"
plan_path: null                   # first-fill only after init
target_size: M
provider: claude                  # claude | codex | gemini
provider_mode: standard
claude_transcript_id: "..."       # foreign-session guard
graph_node_id: ab-xxxxxxxx        # appended when a backlog node is found
# plus skip flags (no_ship, no_external, ...), budget caps, ownership fields
---
```

For the full field list see [architecture/control-plane-loop.md](architecture/control-plane-loop.md) ("The immutable manifest").

#### Completion (external truth, not gates)

There are no completion gates. The gate machinery (gate booleans, the `fno gate` surface, `gate_reality_map.yaml`, phase verifiers) was deleted in the control-plane collapse. A session is done when external reality agrees: a PR exists, CI is green, every required bot has reviewed, plus a budget ceiling. That decision is made by the read-only `fno-agents loop-check` verb (see [architecture/control-plane-loop.md](architecture/control-plane-loop.md)); terminal side effects (ledger record, plan stamp/graduate, handoff) are written by `fno-agents finalize`. The `<promise>` tag signals the agent's belief that work is complete; the loop only terminates when `loop-check` confirms it against the world.

### STATE.md

Wave and task progress for the operator skill.

```yaml
current_wave: 2
completed_tasks:
  - id: "1.1"
    status: SUCCESS
    commit: "abc1234"
  - id: "2.1"
    status: SUCCESS
    commit: "def5678"
in_progress:
  - id: "2.2"
    agent: target
    started: "2026-03-31T10:00:00Z"
```

### 00-INDEX.md

Execution strategy created by the plan skill. Lives in the plan directory.

```yaml
execution_mode: mixed | sequential | parallel
waves:
  - wave: 1
    mode: sequential
    tasks: [1.1]
  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2, 2.3]
  - wave: 3
    mode: sequential
    tasks: [3.1]
```

### SUMMARY.md

Task completion notes written by archer agents. Used for context between waves but never trusted by verification agents.

### CONTEXT.md

User constraints and domain checklist. Read by execution agents at startup.

### current-PLAN.md

Task specification passed to subagents. Contains the specific task an agent should implement.

---

## Configuration

### settings.yaml

Project-level configuration created by `/fno:setup`.

```yaml
workspace:
  projects:
    - path: /path/to/frontend
      name: frontend
      type: nextjs
    - path: /path/to/backend
      name: backend
      type: supabase
  github_org: my-org

reviewer:
  provider: gemini | coderabbit | claude
  auto_request: true

roles:
  - name: "facility admin"
    description: "Manages facility compliance"
```

### Plugin Manifest

**File**: `.claude-plugin/plugin.json`

```json
{
  "name": "footnote",
  "version": "1.0.0",
  "description": "Development workflow that integrates brainstorming, planning, and E2E testing.",
  "author": { "name": "Jason Noah Choi" },
  "keywords": ["testing", "e2e", "playwright", "bdd", "acceptance-criteria",
               "workflow", "planning", "debugging", "code-review"]
}
```

## Orchestrator CLI

The operator skill includes a Python orchestrator for wave-based execution.

**File**: `skills/do/orchestrator.py`

```bash
# Show help
python skills/do/orchestrator.py --help

# Execute a plan
python skills/do/orchestrator.py path/to/00-INDEX.md

# Run a single agent with tags
python skills/do/orchestrator.py --agent "Build React component" --tags ui,frontend
```

### Agent Routing

The orchestrator routes tasks to specialized agents based on keywords in the task description:

| Keywords | Routed Agent |
|----------|-------------|
| frontend, react, ui, component, tailwind | target (frontend profile) |
| backend, api, supabase, auth, database | target (backend profile) |
| devops, docker, ci/cd, deploy, terraform | target (devops profile) |
| etl, pipeline, data, analytics | target (data profile) |

---

## Deviation Rules

When execution agents encounter issues not covered by the plan:

| Situation | Action |
|-----------|--------|
| Bug in plan | Fix inline, note in SUMMARY.md |
| Minor enhancement (under 15 min) | Implement, note it |
| Architecture decision | STOP, return BLOCKED |
| Missing dependency | STOP, return BLOCKED |

---

## Installation

### Development Mode (recommended)

```bash
claude --plugin-dir /path/to/footnote
```

### Permanent Installation

```bash
ln -s /path/to/footnote ~/.claude/plugins/footnote
```

---

## Scripts

Utility scripts in `scripts/`.

| Script | Purpose |
|--------|---------|
| `validate-test-first.sh` | Validates TDD discipline in commits |
| `run-target-loop.sh` | Cross-session external loop runner |
| `metrics/analyze.sh` | Subagent cost and performance analysis |

```bash
# Validate test-first discipline
./scripts/validate-test-first.sh

# Analyze subagent metrics
./scripts/metrics/analyze.sh
```

---

## Skill and Agent Development

### Creating a New Skill

```
skills/<name>/
  SKILL.md              # Main definition (YAML frontmatter + markdown body)
  references/           # Supporting documents (optional)
  scripts/              # Shell scripts (optional)
```

**SKILL.md frontmatter fields**: `name`, `description`, `model`, `context`, `tools`, `skills`

### Creating a New Agent

```
agents/<name>.md
```

**Frontmatter fields**:
- `name` - agent identifier
- `description` - when to use this agent (with examples)
- `model` - sonnet, opus, haiku, or inherit
- `color` - terminal color for output
- `tools` - array of allowed tools
- `disallowedTools` - array of blocked tools (optional)
- `skills` - array of skill names to load (optional)

### Context Forking

Some skills use `context: fork` to run in isolated subprocesses:

| Skill | Model | Rationale |
|-------|-------|-----------|
| `create-pr` | haiku | Mechanical task - read commits, generate PR description |

Forked skills preserve main conversation context for complex work while offloading template-driven tasks to cheaper models.
