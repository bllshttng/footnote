# Design Guidelines

Comprehensive design guidelines for building skills, agents, workflows, and
cross-CLI integrations in the footnote plugin.

---

## Table of Contents

1. [Skill Design Patterns](#skill-design-patterns)
2. [Agent Design Patterns](#agent-design-patterns)
3. [Workflow Design](#workflow-design)
4. [Cross-CLI Portability](#cross-cli-portability)
5. [Iteration Protocol](#iteration-protocol)
6. [Naming Conventions](#naming-conventions)

---

## Skill Design Patterns

### Frontmatter Schema

Every skill lives in `skills/<name>/SKILL.md`. The file begins
with YAML frontmatter delimited by `---` fences, followed by a Markdown body.

#### Required Fields

```yaml
---
name: skill-name          # kebab-case, matches directory name
description: "One-line description. Start with use-case trigger phrases."
---
```

The `description` field serves double duty: it is the skill's summary AND its
invocation trigger. Include natural-language phrases that match how users ask
for the skill. Examples from real skills:

```yaml
# Good - includes trigger phrases
description: "Use when: debugging any bug, test failure, or unexpected behavior."
description: "Autonomous fix loop. One fix per iteration, auto-revert on regression. Use when: 'fix all errors', 'make tests pass'."

# Bad - no trigger context
description: "Fixes things."
```

#### Optional Fields

```yaml
---
name: skill-name
description: "..."
context: main              # main (default) or fork
model: haiku               # haiku, sonnet, or opus (inherits parent if omitted)
argument-hint: "<plan-path> [--flag]"  # Usage hint shown to users
hooks:                     # Skill-level hook declarations
  PreToolUse:
    - matcher: ".*"
      once: true
      hooks:
        - type: command
          command: "${CLAUDE_PLUGIN_ROOT}/hooks/helpers/some-hook.sh"
---
```

#### Frontmatter Conventions

- Keep `name` identical to the directory name. `skills/fix/SKILL.md` has `name: fix`.
- Put the most important trigger phrase first in `description`.
- Use `argument-hint` when the skill takes parameters - it appears in help output.
- Only declare `model` when you need to override the parent session's model.
- Only declare `hooks` when the skill needs lifecycle callbacks (state init, context injection).

### Context Modes

Skills run in one of two context modes:

| Mode | Behavior | Token Cost | Use When |
|------|----------|------------|----------|
| `main` | Runs in the current conversation context | Shared | Complex reasoning, multi-step workflows, interactive skills |
| `fork` | Runs in an isolated subprocess with fresh context | Separate | Mechanical/template tasks, cheap models, no state needed |

#### When to Use `fork`

Fork when ALL of these are true:

1. The task is mechanical (template-driven, no creative judgment)
2. The task does not need conversation history
3. The task can use a cheaper model (haiku)
4. The task produces a discrete output (PR, commit message, report)

Currently only `create-pr` uses fork context:

```yaml
---
name: create-pr
context: fork
model: haiku
---
```

#### When to Stay in `main`

Stay in main (the default) when:

- The skill needs prior conversation context (think, plan, debug)
- The skill orchestrates other skills or agents (target, operator)
- The skill is interactive and asks the user questions (setup, think)
- The skill modifies shared state files (do, fix)

### Model Selection

Choose the cheapest model that can do the job:

| Model | Cost | Use For | Examples |
|-------|------|---------|----------|
| `haiku` | Low | Mechanical tasks, verification, template filling | create-pr, verifier agent |
| `sonnet` | Medium | Implementation, debugging, most execution work | target agent, fix, debug |
| `opus` | High | Code review, architecture decisions, complex reasoning | code-reviewer agent |

**Default behavior:** If no `model` is declared, the skill inherits the parent
session's model. Only override when you have a strong reason.

**Rule of thumb:** If the skill follows a rigid template or checklist, use haiku.
If it writes or modifies code, use sonnet. If it judges code quality, use opus.

### Tool Declarations

Skills do not declare tools in their frontmatter - tools are declared on agents.
Skills describe what tools they need in their Markdown body, and the agent
running the skill provides the tools.

If a skill references tool usage (e.g., "run this bash command"), document it
in the body but do not add a `tools` field to the frontmatter.

### Reference Documents

Split content into `references/` when:

- The content is shared across multiple skills (iteration-loop.md, verification-patterns.md)
- The content is large enough to bloat the main SKILL.md (>200 lines of reference)
- The content changes independently from the skill logic

```
skills/target/
  SKILL.md
  references/
    iteration-loop.md         # Shared with fix, debug
    verification-patterns.md  # Shared with fix, debug, operator
    cli-tool-mapping.md       # Multi-CLI tool equivalents
    state-schema.md           # State file format spec
    cross-project.md          # Cross-project pipeline details
    domain-profiles.md        # Domain-agnostic configuration
```

Reference with relative links from the skill body:

```markdown
## Reference Materials

Load as needed:

- `skills/target/references/iteration-loop.md`
- `skills/target/references/verification-patterns.md`
```

The phrase "Load as needed" signals lazy loading - the LLM reads these only when
the relevant section is reached, saving tokens.

### Compatibility Aliases

When a skill is renamed, create a thin shim that redirects to the canonical name:

```yaml
---
name: operator
description: "Compatibility alias for fno:operator. Use fno:operator for new sessions."
argument-hint: "[expertise] [plan-path]"
---

# Doing Alias

`fno:operator` remains as a compatibility shim.

Use `fno:operator` (`skills/do/references/waves.md`) for the primary flow.

When invoked through this alias, follow the exact same workflow as `fno:operator`.
```

Current aliases:

| Alias | Canonical | Reason |
|-------|-----------|--------|
| `doing` | `operator` | Renamed for clarity |
| `target` | `target` | Simplified name |

**Rules for aliases:**
- Keep the alias SKILL.md under 20 lines (frontmatter + redirect)
- Include a link to the canonical skill
- State "compatibility alias" in the description
- Never duplicate logic - always redirect

### Skill Body Structure

The Markdown body follows a consistent pattern:

```markdown
# Skill Name

One-line summary of what this skill does.

## Reference Materials        # If applicable - lazy-loaded docs

## Defaults                   # Default values, iteration counts

## Interactive Setup          # If the skill asks user questions

## Process                    # The main workflow

### 1. Step Name              # Numbered steps
### 2. Step Name
### 3. Step Name

## Output                     # What the skill produces
```

Use `<HARD-GATE>` tags for non-negotiable rules:

```markdown
<HARD-GATE>
Do NOT invoke any implementation skill until the user has approved the design.
</HARD-GATE>
```

Use `**FORBIDDEN**` markers for enforcement:

```markdown
**FORBIDDEN statuses (will break the loop):**
- `EXECUTION_COMPLETE`, `REVIEW_PENDING`, `AWAITING_MERGE`
```

Soft guidance gets ignored by LLMs. Use FORBIDDEN markers and gate tags for
rules that must actually be followed.

---

## Agent Design Patterns

### Agent Types

Agents live in `agents/<name>.md` and fall into four categories:

| Type | Purpose | Model | Examples |
|------|---------|-------|---------|
| **Execution** | Implements tasks with TDD | sonnet | target, archer |
| **Review** | Judges code quality | opus | code-reviewer |
| **Verification** | Checks completion criteria | haiku | verifier, goal-verifier |
| **Specialized** | Domain-specific analysis | varies | tournament-debugger, silent-failure-hunter, type-design-analyzer |

### Agent Frontmatter Schema

```yaml
---
name: agent-name
description: >
  When to use this agent. Include examples with <example> tags
  showing context, user message, assistant response, and commentary.
model: sonnet                 # haiku, sonnet, or opus
color: cyan                   # Terminal color for output identification
tools: ["Read", "Write", "Edit", "Grep", "Glob", "Bash"]
disallowedTools: ["Task", "WebSearch"]  # Optional - explicit deny list
skills:                       # Skills this agent loads
  - fno:tdd
---
```

### Model Assignment Strategy

| Agent Type | Model | Rationale |
|------------|-------|-----------|
| Execution agents | `sonnet` | Balanced cost/capability for writing code |
| Review agents | `opus` | Needs deep reasoning to catch subtle bugs |
| Verification agents | `haiku` | Checklist-driven, no creative judgment needed |
| Specialized analysis | `sonnet` or `opus` | Depends on reasoning depth required |

### Tool Allowlists by Agent Type

Match tools to what the agent actually needs:

| Agent Type | Tools | Rationale |
|------------|-------|-----------|
| **Execution** | Read, Write, Edit, Grep, Glob, Bash | Full filesystem + shell access for implementation |
| **Review** | Read, Grep, Glob, Bash | Read-only + shell for running tests, no Write/Edit |
| **Verification** | Read, Grep, Glob, Bash | Read-only + shell for verification commands |
| **Specialized** | Varies | Minimal set for the specific analysis task |

**Key restriction:** Execution agents should have `disallowedTools: ["Task"]` to
prevent recursive agent spawning. Only orchestrator-level skills should spawn
subagents.

### Return Contract

Every agent that runs as a subagent MUST return a structured result block:

```
RESULT: SUCCESS|DONE_WITH_CONCERNS|FAILED|BLOCKED
TASK: task-id
COMMIT: abc1234 (if SUCCESS or DONE_WITH_CONCERNS)
CONCERNS: what worries you (if DONE_WITH_CONCERNS)
ERROR: message (if FAILED)
REASON: why (if BLOCKED)
UNBLOCKS_AFTER: what needs to happen (if BLOCKED)
```

#### Result Values

| Result | Meaning | Next Action |
|--------|---------|-------------|
| `SUCCESS` | Task complete, tests pass, committed | Move to next task |
| `DONE_WITH_CONCERNS` | Task complete but with caveats | Orchestrator reviews concerns, may continue |
| `FAILED` | Task could not be completed | Orchestrator may retry or escalate |
| `BLOCKED` | External dependency prevents progress | Orchestrator resolves blocker or reorders |

**Non-negotiable:** Always include `TASK` so the orchestrator can match results
to plan items. Always include `COMMIT` on success so the orchestrator can track
what changed.

### Agent Description Best Practices

Include `<example>` blocks in the description to help the orchestrator understand
when to spawn the agent:

```yaml
description: >
  Use this agent when you need to review code.

  <example>
  Context: The user has just implemented a new feature.
  user: "Can you check if everything looks good?"
  assistant: "I'll launch the code-reviewer agent."
  <commentary>
  Use after feature completion to catch issues before PR.
  </commentary>
  </example>
```

### Spawning Patterns

**Use the Agent/Task tool** (subagent) when:
- The work is independent and can run in parallel
- The work needs a different model than the current session
- You want to limit context exposure (the agent gets a clean context)
- The agent needs specific tool restrictions

**Use inline execution** (no subagent) when:
- The work needs conversation history
- The work is small and sequential
- The overhead of spawning is not justified
- The user is interacting directly

---

## Workflow Design

### Linear Chain

The primary workflow follows a linear chain:

```
think -> plan -> do -> review -> ship
```

| Phase | Skill | Purpose |
|-------|-------|---------|
| **Think** | `/think` | Explore design, generate acceptance criteria |
| **Plan** | `/blueprint` | Create implementation plan with tasks and waves |
| **Do** | `/do` or `/do waves` or `/target` | Execute the plan |
| **Review** | `/review sigma` | Review changes against guidelines |
| **Ship** | `/pr create` | Create PR from commits |

Each phase is independent - you can enter at any point. Have a plan already?
Skip think and plan. Just need a PR? Use `/pr create`.

### Execution Tiers

Three tiers of execution, escalating in complexity and cost:

| Tier | Skill | Use When | State | Agents |
|------|-------|----------|-------|--------|
| **Lightweight** | `/do` | Focused plan, single session, 2-5 files | `.fno/STATE.md` | None (inline) |
| **Heavy** | `/do waves` | Multi-phase plan with waves, parallel tasks | `.fno/STATE.md` + `SUMMARY.md` | Spawns target agents |
| **Autonomous** | `/target` | End-to-end from idea to PR, loop until done | `.fno/target-state.md` | Full pipeline with stop hook |

#### Decision Tree

```
Is it a quick focused change (< 1 session)?
  YES -> /do path/to/plan.md
  NO  -> Does the plan have waves/parallel tasks?
    YES -> /do waves path/to/plan/
    NO  -> Do you want full autonomy (idea to PR)?
      YES -> /target "feature description"
      NO  -> /do waves path/to/plan/
```

### Quality Gate Pattern

Quality gates are checkpoints that MUST pass before proceeding. Define gates
explicitly in skills:

#### Gate Types

| Gate | Checks | Enforced By |
|------|--------|-------------|
| **Pre-flight** | Environment ready, deps installed, tools available | Agent startup protocol |
| **Red-Green** | Test fails before implementation, passes after | TDD discipline in target agent |
| **Build** | Project compiles/builds without errors | Verification patterns |
| **Test** | All tests pass (unit + integration) | Guard command in iteration loop |
| **Review** | Code meets project guidelines | code-reviewer agent |
| **CI** | Remote CI pipeline passes | create-pr pre-checks |

#### Defining Custom Gates

In plan files, define gates per wave:

```yaml
waves:
  - wave: 1
    mode: sequential
    tasks: [1.1]
    gate: "npm run build && npm test"
  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2, 2.3]
    gate: "npm run test:integration"
```

### State File Conventions

#### Persistent State (tracked in `.fno/`)

| File | Owner | Purpose | Lifetime |
|------|-------|---------|----------|
| `target-state.md` | target | Pipeline iteration tracking, status, phase | Duration of target loop |
| `STATE.md` | do waves | Wave and task progress | Duration of plan execution |
| `SUMMARY.md` | do waves agents | Task completion notes, concerns | Duration of plan execution |
| `00-INDEX.md` | blueprint | Execution strategy, wave definitions | Permanent (part of plan) |
| `current-PLAN.md` | do waves | Current task spec for subagent | Per-task (overwritten) |
| `CONTEXT.md` | blueprint | User constraints and domain checklist | Duration of plan execution |

#### Ephemeral State (not tracked)

| File | Purpose | Lifetime |
|------|---------|----------|
| `observations.jsonl` | Raw feel captures (gitignored) | Session |
| Debug hypothesis files | Individual debug iterations | Debug session |
| Fix iteration logs | Individual fix attempts | Fix session |

**Rule:** State files use Markdown with YAML frontmatter for machine readability.
The stop hook and orchestrator parse these files, so format matters.

### Deviation Rules

When an agent encounters something not in the plan:

| Situation | Action | Report |
|-----------|--------|--------|
| Bug in plan (wrong assumption) | Fix inline, implement correct approach | Note in SUMMARY.md |
| Minor enhancement (<15 min) | Implement it, keep moving | Note in SUMMARY.md |
| Architecture decision needed | **STOP** | Return BLOCKED with reason |
| Missing dependency | **STOP** | Return BLOCKED with UNBLOCKS_AFTER |
| Scope creep (>15 min) | **STOP** | Return BLOCKED, suggest separate task |

**The key principle:** Agents have autonomy for tactical decisions but must defer
strategic decisions. "Should I use a different data structure?" is tactical.
"Should we use a different database?" is strategic.

---

## Cross-CLI Portability

### Architecture

The footnote plugin supports three CLI platforms:

| Platform | Skills | Hooks | Agents |
|----------|--------|-------|--------|
| Claude Code | Native | `hooks.json` | Native via Agent tool |
| Gemini CLI | Portable | `hooks-gemini.json` | Synced via `sync-gemini-agents.py` |
| Codex CLI | Portable | `hooks-codex.json` | Synced via `sync-codex-agents.py` |

### Skills Are Portable

Skills are plain Markdown files with YAML frontmatter. They work identically
across all three CLIs because:

- No platform-specific APIs in skill definitions
- Tool names are abstract (Read, Write, Edit, Bash) and map to each CLI's equivalents
- State files use standard Markdown/YAML formats

The `cli-tool-mapping.md` reference documents tool equivalents across platforms.

### Hooks Are Platform-Specific

Each CLI has its own hook configuration:

```
hooks/
  hooks.json            # Claude Code hooks
  hooks-gemini.json     # Gemini CLI hooks
  hooks-codex.json      # Codex CLI hooks
```

Hook scripts are shared when possible (bash scripts), but the configuration
format and lifecycle events differ per platform.

Key hooks and their cross-CLI support:

| Hook | Claude Code | Gemini CLI | Codex CLI |
|------|-------------|------------|-----------|
| Stop hook (blocks exit) | Native | Soft fallback | Native |
| Session start (context inject) | Native | Native | Native |
| PreToolUse (state init) | Native | Native | Native |

### Agent Sync Scripts

Agents are defined once in `agents/` (Claude Code format) and
synced to other CLIs:

```bash
# Sync to Gemini CLI format
python scripts/sync-gemini-agents.py

# Sync to Codex CLI format
python scripts/sync-codex-agents.py
```

These scripts translate:
- Agent frontmatter to platform-specific config
- Tool names to platform equivalents
- Model names to provider model IDs

### Runtime Configuration

The static `runtime/provider-capabilities.yaml` capability matrix and
`runtime/target-adapters.yaml` phase map that previously drove
provider-aware behavior were removed (multi-CLI adapters superseded by
hooks). Provider behavior is now hook-driven: the hooks layer handles
session hydration, lifecycle enforcement, and parallel-wave decisions
(the operator downgrades a parallel wave to sequential only on detected
file or shared-output conflicts, not from a static capability flag).

### Portability Principle

When building new skills or modifying existing ones:

1. **Never** hardcode CLI-specific behavior in SKILL.md
2. **Always** put provider-specific lifecycle behavior in hooks rather than baking assumptions into skills
3. Put platform-specific logic in hooks or adapter scripts, not in skills
4. Use the `cli-tool-mapping.md` reference for tool name translation
5. Test that skill Markdown reads correctly on all three CLIs

---

## Iteration Protocol

### Core Loop

All bounded iteration in footnote follows one protocol:

```
do ONE thing -> verify mechanically -> keep or discard -> repeat
```

This protocol is defined in `skills/target/references/iteration-loop.md` and
shared by fix, debug, target, and any skill that needs bounded exploration.

### Required Seed

Before iteration 1, every loop must define:

| Parameter | Purpose | Example |
|-----------|---------|---------|
| **Goal** | What the loop optimizes for | "All type errors resolved" |
| **Scope** | Files, systems, or dimensions in bounds | "src/**/*.ts" |
| **Metric** | Mechanical success criterion | "tsc --noEmit exit code 0" |
| **Verify command** | Extracts pass/fail or numeric delta | "npm run typecheck 2>&1 \| tail -1" |
| **Guard command** | Regression check that must always pass | "npm test" |
| **Iterations** | Exact count for bounded mode | 15 (fix), 5 (debug) |

### Loop Sequence

Each iteration:

1. **Seed** - Restate the current target. Confirm verify and guard commands.
2. **Do ONE thing** - Make one atomic change. Never batch multiple fixes.
3. **Verify** - Run the verify command. Record pass/fail.
4. **Guard** - Run the guard command. If it fails, the change introduced a regression.
5. **Keep or Discard** - If verify passes and guard passes, commit. Otherwise revert.
6. **Log** - Record what was tried and the result.

### Fix Loop

The fix skill uses bounded iteration with these specifics:

| Parameter | Value |
|-----------|-------|
| Default iterations | 15 |
| Output root | `fix/{YYMMDD}-{HHMM}-{slug}/` |
| Revert policy | Auto-revert on guard failure (regression) |
| Prioritization | Build failures > type errors > test failures > lint > warnings |
| Chaining | Can chain from debug findings via `--from-debug` |

### Debug Loop

The debug skill uses hypothesis-based iteration:

| Parameter | Value |
|-----------|-------|
| Max hypotheses | 5 |
| Method | Scientific method - one hypothesis per iteration |
| Prerequisite | BDD acceptance criteria + failing test BEFORE investigation |
| Escalation | Tournament pattern - competing hypotheses, evidence decides |
| Output | `.debug/bugs/YYYY-MM-DD-bug-name.md` |

### Target Loop

The target autonomous loop uses a stop hook to enforce completion:

| Parameter | Value |
|-----------|-------|
| State file | `.fno/target-state.md` |
| Status values | `IN_PROGRESS`, `COMPLETE`, `BLOCKED` (only these three) |
| Completion signal | `<promise>MISSION COMPLETE: ...</promise>` tag in output |
| Stop hook | Blocks session exit when status is `IN_PROGRESS` and no promise tag |
| External loop | `scripts/run-target-loop.sh` re-invokes CLI across sessions |

**The Golden Rule:** Status remains `IN_PROGRESS` until the model outputs a
`<promise>` tag. The stop hook enforces this mechanically - no trust involved.

**Forbidden statuses:** `EXECUTION_COMPLETE`, `REVIEW_PENDING`, `AWAITING_MERGE`,
`DONE`, or any other invented status. These will break the stop hook.

### Anti-Patterns

| Anti-Pattern | Why It Fails | Correct Approach |
|--------------|-------------|------------------|
| Batching multiple fixes in one iteration | Cannot identify which fix caused regression | One change per iteration |
| "Looks better" verification | Subjective, unreliable | Mechanical verify command |
| Skipping the guard command | Silent regressions accumulate | Always run guard after verify |
| Exceeding iteration budget | Diminishing returns, token waste | Stop at budget, report findings |
| Not committing kept changes | Later iterations cannot build on progress | Commit after each kept change |

---

## Naming Conventions

### Skills

| Element | Convention | Example |
|---------|-----------|---------|
| Directory | kebab-case | `skills/target/` |
| Main file | Always `SKILL.md` (uppercase) | `skills/fix/SKILL.md` |
| References dir | Always `references/` | `skills/target/references/` |
| Reference files | kebab-case `.md` | `iteration-loop.md` |
| Scripts dir | Always `scripts/` | `skills/do/scripts/` |
| Script files | kebab-case `.sh` or `.py` | `orchestrator.py` |

### Agents

| Element | Convention | Example |
|---------|-----------|---------|
| File | kebab-case `.md` | `agents/code-reviewer.md` |
| Name field | kebab-case matching filename | `name: code-reviewer` |
| Color | Distinct per agent for terminal output | `color: green` |

### Commands

| Element | Convention | Example |
|---------|-----------|---------|
| Slash command | kebab-case matching skill name | `/fno:target` |
| Plugin prefix | `footnote:` | `/fno:fix` |
| Arguments | Positional first, flags after | `/target backend "add auth"` |

### Hooks

| Element | Convention | Example |
|---------|-----------|---------|
| Config files | `hooks.json`, `hooks-{platform}.json` | `hooks-gemini.json` |
| Hook scripts | kebab-case `.sh` | `target-stop-hook.sh` |
| Helper scripts | In `hooks/helpers/` | `hooks/helpers/init-target-state.sh` |

### State Files

| Element | Convention | Example |
|---------|-----------|---------|
| Directory | `.fno/` in project root | `.fno/target-state.md` |
| Files | UPPER-CASE `.md` for important state | `STATE.md`, `SUMMARY.md` |
| Files | kebab-case `.md` for specific state | `target-state.md` |
| Plan index | Always `00-INDEX.md` | `.fno/00-INDEX.md` |

### General Rules

1. **Never use spaces** in file or directory names
2. **Never use camelCase** for file names (use kebab-case)
3. **UPPER-CASE** is reserved for important documents: `SKILL.md`, `CLAUDE.md`, `STATE.md`, `SUMMARY.md`
4. **Match names across layers** - if the skill is `fix`, the command is `/fix`, the directory is `skills/fix/`
5. **Aliases match their canonical name's pattern** - `target` alias points to `target`
