# footnote Plugin - Code Standards

## Skill Structure

Every skill is a directory under `skills/` containing at minimum a `SKILL.md` file.

### Directory Layout

```
skills/<skill-name>/
├── SKILL.md              # Main skill definition (required)
├── CLAUDE.md             # Skill-level learned context (auto-generated)
├── references/           # Supporting documentation (optional)
│   ├── template.md       # Templates used by the skill
│   ├── examples.md       # Example inputs/outputs
│   └── cli-tool-mapping.md  # Multi-CLI tool equivalents
└── scripts/              # Shell scripts (optional)
    └── helper.sh         # Automation scripts
```

### SKILL.md Format

Every `SKILL.md` has two parts: YAML frontmatter and a markdown body.

```markdown
---
name: skill-name
description: "Short description. Use when: trigger phrases for this skill."
argument-hint: "<required-arg> [--optional-flag]"
hooks:
  PreToolUse:
    - matcher: ".*"
      once: true
      hooks:
        - type: command
          command: "${CLAUDE_PLUGIN_ROOT}/hooks/helpers/init-session-state.sh"
---

# Skill Title

Markdown body with instructions, process steps, templates, and rules.
```

### Frontmatter Fields Reference

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Skill identifier (kebab-case) |
| `description` | string | Yes | Short description with trigger phrases |
| `argument-hint` | string | No | Usage hint shown in command palette |
| `context` | string | No | `fork` for isolated subprocess execution |
| `model` | string | No | Override model (e.g., `haiku`, `sonnet`, `opus`) |
| `tools` | list | No | Allowed tools for forked context |
| `disallowedTools` | list | No | Explicitly blocked tools |
| `hooks` | object | No | Lifecycle hooks (PreToolUse, PostToolUse, etc.) |
| `hooks.PreToolUse` | list | No | Hooks triggered before tool invocation |
| `hooks.PreToolUse[].matcher` | string | No | Regex pattern for tool name matching |
| `hooks.PreToolUse[].once` | boolean | No | Fire only on first match |
| `hooks.PreToolUse[].hooks` | list | No | Hook actions to execute |

### Skill Body Conventions

- Start with a level-1 heading matching the skill name
- Use `<HARD-GATE>` tags for non-negotiable rules that must block all progress
- Use `<FORBIDDEN>` markers for actions that must never be taken
- Document the process as numbered steps
- Include tables for decision matrices and agent routing
- Reference supporting docs via relative paths within the skill: `[Label](references/topic.md)`
- Keep total skill size under 20k tokens (including references)

## Agent Structure

Agents are single `.md` files under `agents/`. Each agent is a specialized persona with constrained tools and skills.

### Agent File Format

```markdown
---
name: agent-name
description: Short description of what this agent does and when to use it.
model: sonnet
color: cyan
tools: ["Read", "Write", "Edit", "Grep", "Glob", "Bash"]
disallowedTools: ["Task", "WebSearch", "WebFetch"]
skills:
  - fno:tdd
---

You are agent-name - a disciplined [role]. Your job is to [primary responsibility].

## Context Efficiency

You are running in a **subagent context** with limited resources. Be efficient:
- Don't explore unnecessarily
- Don't spawn subagents (no Task tool)
- Work from the plan
- Commit atomically

## Startup Protocol

1. Read `.fno/current-PLAN.md`
2. Read `.fno/CONTEXT.md` (if exists)
3. Read `.fno/STATE.md` (if exists)
4. Run pre-flight checks

[... agent-specific instructions ...]
```

### Agent Frontmatter Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Agent identifier (kebab-case) |
| `description` | string | Yes | What the agent does, with usage examples |
| `model` | string | Yes | Model tier: `haiku`, `sonnet`, or `opus` |
| `color` | string | No | Terminal color for agent output |
| `tools` | list | Yes | Allowed tools (whitelist) |
| `disallowedTools` | list | No | Explicitly blocked tools (blacklist) |
| `skills` | list | No | Skills this agent can invoke |

### Model Selection by Agent Role

| Role | Model | Rationale |
|------|-------|-----------|
| Code review (deep analysis) | opus | Needs nuanced understanding |
| Task execution (implementation) | sonnet | Balance of capability and cost |
| Verification (mechanical checks) | haiku | Simple pass/fail checks |
| PR creation (template-driven) | haiku | Mechanical text generation |

## Return Contract for Agents

All archer/execution agents must return a structured result when they complete. This contract is how the orchestrator tracks progress.

### Required Fields

```
RESULT: SUCCESS|DONE_WITH_CONCERNS|FAILED|BLOCKED
TASK: <task-id>
```

### Fields by Result Type

| Result | Required Fields | Description |
|--------|----------------|-------------|
| `SUCCESS` | `TASK`, `COMMIT` | Task completed, tests pass, committed |
| `DONE_WITH_CONCERNS` | `TASK`, `COMMIT`, `CONCERNS` | Task completed but with noted issues |
| `FAILED` | `TASK`, `ERROR` | Task could not be completed |
| `BLOCKED` | `TASK`, `REASON`, `UNBLOCKS_AFTER` | Task cannot proceed, needs external action |

### Examples

**Success:**
```
RESULT: SUCCESS
TASK: 2.1
COMMIT: a1b2c3d
```

**Done with concerns:**
```
RESULT: DONE_WITH_CONCERNS
TASK: 2.2
COMMIT: d4e5f6a
CONCERNS: Database migration works but adds ~200ms latency on cold start. Consider connection pooling.
```

**Failed:**
```
RESULT: FAILED
TASK: 3.1
ERROR: Test runner not found. Expected pytest but project uses vitest.
```

**Blocked:**
```
RESULT: BLOCKED
TASK: 2.3
REASON: Requires Stripe API key which is not configured in .env
UNBLOCKS_AFTER: User adds STRIPE_SECRET_KEY to .env
```

## Naming Conventions

### Files and Directories

| Scope | Convention | Examples |
|-------|-----------|----------|
| Skill directories | kebab-case | `sigma-review/`, `target/`, `check-pr/` |
| Agent files | kebab-case | `code-reviewer.md`, `silent-failure-hunter.md` |
| Shell scripts | kebab-case | `target-stop-hook.sh`, `validate-test-first.sh` |
| Python scripts | kebab-case (files), snake_case (variables/functions) | `roadmap-tasks.py` |
| JSON configs | kebab-case | `hooks-gemini.json` |
| State files | UPPER_CASE | `STATE.md`, `SUMMARY.md`, `CONTEXT.md` |
| Index files | 00-prefixed | `00-INDEX.md` |

### Code Identifiers

| Language | Convention | Examples |
|----------|-----------|----------|
| Shell variables | UPPER_SNAKE | `TARGET_STATE`, `PLUGIN_ROOT` |
| Shell functions | lower_snake or kebab | `check_status()`, `init-state()` |
| Python variables | snake_case | `task_id`, `wave_config` |
| Python functions | snake_case | `parse_index()`, `route_task()` |
| Python classes | PascalCase | `WaveOrchestrator`, `TaskRouter` |
| YAML keys | snake_case or kebab-case | `execution_mode`, `argument-hint` |
| Hook events | PascalCase | `PreToolUse`, `PostToolUse`, `SessionStart` |

**Note:** camelCase is generally avoided for internal identifiers, though it appears in specific manifest or configuration fields (e.g., `disallowedTools`, `pluginRoot`).

## Shell Script Standards

### Preamble

All shell scripts start with:

```bash
#!/usr/bin/env bash
set -euo pipefail
```

- `set -e` - exit on error
- `set -u` - error on undefined variables
- `set -o pipefail` - propagate pipe failures

### Patterns

**Prefer functions over separate script files** when logic is tightly coupled:

```bash
check_target_state() {
  local state_file="$1"
  if [[ ! -f "$state_file" ]]; then
    echo "NO_STATE"
    return
  fi
  # ...
}
```

**Use local variables** inside functions:

```bash
my_function() {
  local input="$1"
  local result
  result=$(process "$input")
  echo "$result"
}
```

**Quote all variable expansions:**

```bash
# Correct
if [[ -f "$state_file" ]]; then

# Wrong
if [[ -f $state_file ]]; then
```

**Use `[[ ]]` over `[ ]`** for conditionals (bash-specific, safer):

```bash
# Correct
if [[ "$status" == "IN_PROGRESS" ]]; then

# Wrong
if [ "$status" = "IN_PROGRESS" ]; then
```

**Exit codes matter** - hooks and callers check them:

```bash
# 0 = success/allow
# 1 = failure/block
# 2 = error (distinct from intentional block)
```

### Hook Scripts

Hook scripts have additional requirements:

- Must be executable (`chmod +x`)
- Must handle missing state files gracefully (the hook might fire before state is initialized)
- Must exit quickly - hooks run synchronously and block the CLI
- Must read environment variables set by the CLI (`CLAUDE_PLUGIN_ROOT`, etc.)
- stdout is captured - use stderr for debug logging

## Commit Conventions

The project uses conventional commits:

```
<type>: <description>

[optional body]
```

### Types

| Type | When to Use |
|------|-------------|
| `feat` | New feature or capability |
| `fix` | Bug fix |
| `refactor` | Code restructuring without behavior change |
| `docs` | Documentation only |
| `chore` | Maintenance, deps, CI, tooling |
| `test` | Adding or updating tests |

### Examples

```
feat: add tournament debugging with competitive hypothesis testing
fix: preserve gemini workspace upgrade detection
refactor: rename orchestration commands and executor agent
docs: add architecture decision record for wave orchestration
chore: sync codex agent definitions
```

### Commit Scope

- One task = one commit (atomic)
- Commit messages describe the "why" not the "what"
- Keep the subject line under 72 characters
- No emdashes in commit messages (use hyphens)

**NON-NEGOTIABLE for Medium and Large size profiles:** Each completed task or wave in `/do waves` MUST produce a commit before execution moves to the next task. Never accumulate all changes into a single kitchen-sink commit at the end. Stage only files relevant to the task (never `git add .` or `git add -A`). Include a task reference in the commit body:

```
feat|fix|refactor(scope): what changed

Task N.M: task title from plan
```

Large undifferentiated diffs are hard to review, hard to revert, and break `git bisect`. For Small (`-S`) profiles, atomic commits per task are encouraged but not enforced.

## The No-Emdash Rule

**Never use emdashes (U+2014) in any output.** This includes:

- Code and comments
- Shell scripts and strings
- Documentation and markdown
- Commit messages
- Plan files
- Conversation text

**Why:** Emdashes break single-quoted shell strings and cause encoding issues across toolchains.

**Instead:** Use a regular hyphen-dash (`-`) or rewrite the sentence.

```bash
# Wrong - emdash will break this
echo 'This is the state — active'

# Correct - regular hyphen
echo 'This is the state - active'
```

## Deviation Rules

When a target agent encounters something not in the plan, these rules determine whether to stop or continue:

### Fix Inline (Do Not Stop)

| Situation | Action |
|-----------|--------|
| Bug discovered in the plan itself | Fix inline, note in SUMMARY.md |
| Minor enhancement (under 15 min) | Implement, note in SUMMARY.md |
| Missing import or typo | Fix immediately |
| Test needs adjustment for reality | Update test, document why |

### Stop and Return BLOCKED

| Situation | Action |
|-----------|--------|
| Architecture decision required | STOP, return BLOCKED with REASON |
| Missing dependency (API key, service) | STOP, return BLOCKED with UNBLOCKS_AFTER |
| Scope significantly larger than planned | STOP, return BLOCKED |
| Conflicting requirements discovered | STOP, return BLOCKED |
| Need human judgment on UX/product | STOP, return BLOCKED |

### The Decision Heuristic

> If you can fix it without changing the plan's intent, fix it.
> If fixing it would change what gets shipped, stop.

## Promise Tag Pattern

Autonomous loops (target, megawalk) complete when the output contains a `<promise>` tag. This is the only mechanism for signaling loop completion.

### Format

```
<promise>MISSION COMPLETE: all tasks done, tests passing, review feedback addressed, PR created</promise>
```

### Rules

1. **Only output `<promise>` when the work is genuinely done** - tests pass, the PR is up, review feedback is addressed
2. The stop hook runs `fno-agents loop-check`, which reads the world (PR + CI + required-bot review) on seeing a promise; it allows exit only when those reads agree
3. The promise is the agent's claim of completion, not the authority - a premature promise blocks (loop-check names the failing read) and the loop continues
4. The promise text should summarize what was accomplished

There are no completion gates to set. The gate machinery (gate booleans in `target-state.md`, the `fno gate` surface, `gate_reality_map.yaml`, phase verifiers) was deleted in the control-plane collapse. Nothing the agent writes to a local file is a precondition of `<promise>`; completion is decided by external reads plus a budget ceiling. See [architecture/control-plane-loop.md](architecture/control-plane-loop.md).

### Termination

`fno-agents loop-check` resolves a session with one `TerminationReason`: `DonePRGreen` / `DoneAdvisory` (work confirmed done), `NoWork`, `Budget`, `NoProgress`, `Interrupted` (cancel sentinel), or `Aborted`. The pre-wedge `IN_PROGRESS` / `COMPLETE` / `BLOCKED` status strings no longer drive the decision.

**Forbidden statuses** (will break the loop):
- `EXECUTION_COMPLETE`
- `REVIEW_PENDING`
- `AWAITING_MERGE`
- `DONE`
- Any other invented status

## Iteration Loop Protocol

Skills that use bounded iteration share a common protocol:

```
do ONE thing -> verify mechanically -> keep or discard -> repeat
```

### Rules

1. **One change per iteration** - never batch multiple fixes
2. **Mechanical verification** - run tests, check exit codes, not vibes
3. **Auto-revert on regression** - if the fix breaks something else, revert it
4. **Bounded** - all loops have a maximum iteration count
5. **Track attempts** - record what was tried to avoid repeating failed approaches

### Fix Loop Specific

The `/fix` skill follows this precisely:

1. Identify ONE issue
2. Apply ONE fix
3. Run the full test suite
4. If tests pass: commit and continue
5. If tests regress: `git checkout -- .` (revert) and try different approach
6. If max iterations reached: stop and report

## Frontmatter Quick Reference

### Skill Frontmatter (SKILL.md)

```yaml
---
name: skill-name                    # Required: kebab-case identifier
description: "Use when: ..."        # Required: trigger description
argument-hint: "<arg> [--flag]"     # Optional: usage hint
context: fork                       # Optional: isolated subprocess
model: haiku                        # Optional: model override
tools: ["Read", "Bash"]             # Optional: tool whitelist (forked only)
disallowedTools: ["Task"]           # Optional: tool blacklist
hooks:                              # Optional: lifecycle hooks
  PreToolUse:
    - matcher: ".*"
      once: true
      hooks:
        - type: command
          command: "script.sh"
---
```

### Agent Frontmatter (agents/*.md)

```yaml
---
name: agent-name                    # Required: kebab-case identifier
description: What it does           # Required: role description
model: sonnet                       # Required: haiku|sonnet|opus
color: cyan                         # Optional: terminal output color
tools: ["Read", "Write", ...]       # Required: allowed tools
disallowedTools: ["Task", ...]      # Optional: blocked tools
skills:                             # Optional: invokable skills
  - fno:tdd
  - fno:write-tests
---
```

### Wave Index (00-INDEX.md)

```yaml
execution_mode: mixed               # sequential | parallel | mixed
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

## Code Review Standards

When the `/review sigma` skill runs, it dispatches 6 specialized agents. Code should be written to pass all of them:

| Agent | What It Checks |
|-------|---------------|
| **code-reviewer** | CLAUDE.md compliance, obvious bugs, code quality, naming |
| **type-design-analyzer** | Type invariants, encapsulation, proper use of generics/interfaces |
| **integration-test-analyzer** | End-to-end journey tests, database state verification |
| **silent-failure-hunter** | Swallowed errors, empty catch blocks, missing error handlers |
| **ux-flow-tester** | User journey simulation, UI state correctness |
| **multi-device-checker** | Responsive behavior, cross-device compatibility |

### Writing Code That Passes Review

- Never swallow errors - always handle or propagate
- Write integration tests that verify database state, not just API responses
- Test UI flows end-to-end, not just individual components
- Check responsive behavior at multiple breakpoints
- Use proper TypeScript types (no `any`, proper generics)
- Follow the project's CLAUDE.md conventions exactly
