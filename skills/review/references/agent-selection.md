# Agent Selection Matrix

## Step 1: Detect Diff Size (Review Tier)

**Right-size reviews.** A 10-line bug fix doesn't need 9 parallel agents.
We tier the review depth to match the change scope — light for small fixes,
full for large features. Automated checks (typecheck, lint, build) always
run regardless of tier.

```bash
# Count lines changed (insertions + deletions)
DIFF_LINES=$(git diff --stat origin/main...HEAD | tail -1 | grep -oE '[0-9]+ insertion|[0-9]+ deletion' | awk '{sum += $1} END {print sum+0}')

# Determine tier
if [[ "$DIFF_LINES" -lt 50 ]]; then
  REVIEW_TIER="light"     # Small change — 2 agents
elif [[ "$DIFF_LINES" -lt 200 ]]; then
  REVIEW_TIER="standard"  # Medium change — 4 agents
else
  REVIEW_TIER="full"      # Large change — all applicable agents
fi
```

### Tier Summary

| Tier | Diff Size | Base Agents | Conditional Agents | Estimated Token Cost |
|------|-----------|-------------|-------------------|---------------------|
| **light** | < 50 lines | code-reviewer, silent-failure-hunter | None | ~2,000 tokens |
| **standard** | 50-200 lines | code-reviewer, silent-failure-hunter | +2 based on change type | ~4,000 tokens |
| **full** | 200+ lines | code-reviewer, silent-failure-hunter | All applicable | ~8,000 tokens |

**Light (< 50 lines):**
- code-reviewer (CLAUDE.md compliance + bugs)
- silent-failure-hunter (error handling)
- Skip: type-design-analyzer, ux-flow-tester, multi-device-checker, integration-test-analyzer
- Still run: all automated checks (typecheck, lint, build)

**Standard (50-200 lines):**
- code-reviewer + silent-failure-hunter (always)
- If frontend: +ux-flow-tester
- If backend: +integration-test-analyzer
- Skip: multi-device-checker, type-design-analyzer

**Full (200+ lines):**
- Current behavior — all applicable agents based on change type

## Step 2: Detect Change Type

```bash
# Get changed files
CHANGED=$(git diff --name-only origin/main...HEAD)

# Detect categories
HAS_FRONTEND=$(echo "$CHANGED" | grep -E '^src/(components|hooks|app)/' && echo "true" || echo "false")
HAS_BACKEND=$(echo "$CHANGED" | grep -E '^src/(server|lib)/' && echo "true" || echo "false")
HAS_SHELL=$(echo "$CHANGED" | grep -E '\.sh$' && echo "true" || echo "false")
HAS_DOCS_ONLY=$(echo "$CHANGED" | grep -vE '\.(md|txt|json)$' | wc -l | tr -d ' ')

# Determine change type
if [[ "$HAS_DOCS_ONLY" == "0" ]]; then
  CHANGE_TYPE="docs-only"
elif [[ "$HAS_FRONTEND" == "true" && "$HAS_BACKEND" == "true" ]]; then
  CHANGE_TYPE="full-stack"
elif [[ "$HAS_FRONTEND" == "true" ]]; then
  CHANGE_TYPE="frontend"
elif [[ "$HAS_BACKEND" == "true" ]]; then
  CHANGE_TYPE="backend"
else
  CHANGE_TYPE="other"
fi
```

## Selection Matrix (Tier × Change Type)

| Change Type | Base Agents (Always) | Conditional Agents | Browser Testing |
|-------------|---------------------|-------------------|-----------------|
| **frontend** | silent-failure-hunter, code-reviewer | ux-flow-tester, multi-device-checker | Yes |
| **backend** | silent-failure-hunter, code-reviewer | type-design-analyzer, integration-test-analyzer | No |
| **full-stack** | silent-failure-hunter, code-reviewer | All conditional agents | Yes |
| **docs-only** | silent-failure-hunter, code-reviewer | None | No |
| **other** | silent-failure-hunter, code-reviewer | Based on file types | Maybe |

## Base Agents (Always Run)

```typescript
// ALWAYS run these two agents
Task({
  subagent_type: "fno:silent-failure-hunter",
  prompt: `Hunt for silent failures in these changes: ${changedFiles}`,
  description: "Hunt silent failures"
})

Task({
  subagent_type: "fno:code-reviewer",
  prompt: `Review code quality for: ${changedFiles}`,
  description: "Review code quality"
})
```

## Git History Agent (Standard + Full Tiers Only)

For standard and full review tiers, run a git history context agent alongside base agents:

```bash
# Only for standard (50-200 lines) and full (200+ lines) tiers
if [[ "$REVIEW_TIER" != "light" ]]; then
  # Spawn an Explore agent to analyze git history context
  # Agent should:
  #   1. Run git blame on each modified file
  #   2. Check for previous PR comments: gh api repos/$OWNER/$REPO/pulls?state=closed&per_page=10
  #   3. Flag patterns from prior reviews that may apply to current changes
  echo "Running git history analysis on: $CHANGED"
fi
```

This agent catches recurring issues — if a reviewer flagged the same pattern on a previous PR touching these files, it's likely relevant again.

## Conditional Agents

**If CHANGE_TYPE includes frontend** (frontend or full-stack):
```typescript
Task({
  subagent_type: "fno:ux-flow-tester",
  prompt: `Test user journeys for: ${frontendFiles}`,
  description: "Test UX flows"
})

Task({
  subagent_type: "fno:multi-device-checker",
  prompt: `Check responsive design for: ${frontendFiles}`,
  description: "Check multi-device"
})
```

**If CHANGE_TYPE includes backend** (backend or full-stack):
```typescript
Task({
  subagent_type: "fno:type-design-analyzer",
  prompt: `Analyze type design for: ${backendFiles}`,
  description: "Analyze types"
})

Task({
  subagent_type: "fno:integration-test-analyzer",
  prompt: `Analyze integration tests for: ${backendFiles}`,
  description: "Analyze integration tests"
})
```

**If CHANGE_TYPE is docs-only:**
- Skip code analysis agents
- Report: "Documentation changes only - limited code review applicable"

**If shell scripts detected:**
- Note in report: "Shell scripts detected - manual bash review recommended"

## Browser Testing (Frontend Only)

```bash
# Check if agent-browser is available
if ! command -v agent-browser &>/dev/null; then
  echo "⚠️  agent-browser not installed — skipping browser testing"
  echo "   Install: npm install -g agent-browser && agent-browser install"
  # Skip browser testing section, note in report
else
  # Use agent-browser for exploratory testing
  agent-browser open http://localhost:3000/app/feature
  agent-browser snapshot -i
  agent-browser screenshot feature-state.png

  # Multi-device testing
  agent-browser set device "iPhone 14"
  agent-browser open http://localhost:3000/app/feature
  agent-browser screenshot mobile.png
fi
```

## Agent Execution and Waiting

**CRITICAL: You MUST wait for ALL spawned agents to complete before proceeding.**

When running agents in parallel:
1. Spawn all agents using Task tool (may use `run_in_background: true` for speed)
2. If running in background, you MUST use `TaskOutput` to wait for EACH agent's result.
3. Collect all results into a single data structure.
4. Do NOT proceed to "Automated Checks" or "Generate Report" until all results are collected. THEN, synthesize the results.

Failing to wait creates orphaned notifications that interrupt the caller's workflow.

## Automated Checks (Always Run)

```bash
# Type check
bun run typecheck

# Lint
bun run lint

# Journey tests (if they exist)
bun run test:journeys 2>/dev/null || echo "No journey tests configured"

# Integration tests (if they exist)
bun run test:integration 2>/dev/null || echo "No integration tests configured"

# Build check
bun run build
```

## Handling Agent Results

### When Agents Disagree
If two agents report conflicting findings:
1. Trust the more specific agent (silent-failure-hunter > code-reviewer on error handling)
2. If both are equally specific, investigate independently
3. Report the conflict to the user — don't silently pick one

### When Agents Return Empty
An agent finding "no issues" on changed files should trigger a sanity check:
- Did the agent actually examine the right files?
- Are the changes in a domain the agent understands?
- For silent-failure-hunter: are there try/catch blocks that SHOULD have been flagged?

### When Automated Checks Fail
If `pnpm test` or `pnpm typecheck` fails:
- STOP the review — don't proceed to browser testing
- Report the failure prominently (not buried in the report)
- The review verdict MUST be "FAIL" regardless of agent results

## Per-Agent Provider Pinning

Each agent in the selection matrix can be pinned to a specific LLM provider via `config.agents.<name>.provider` in `.fno/settings.yaml`. This is Spec 3 of the provider rotation initiative (`ab-978e93ed`).

### Config schema

```yaml
config:
  agents:
    code-reviewer:
      provider: claude-anthropic
    silent-failure-hunter:
      provider: gemini-pro-1
    type-design-analyzer:
      provider: glm-zhipu
```

The `<name>` key must exactly match the `subagent_type` string passed to `Task()` (case-sensitive). The valid names are the agent identifiers used throughout this document: `code-reviewer`, `silent-failure-hunter`, `type-design-analyzer`, `ux-flow-tester`, `multi-device-checker`, `integration-test-analyzer`.

### Back-compat default

When `config.agents.<name>` is unset for an agent, that agent resolves to the globally active provider's id and cli. The `resolve_agent_provider(name)` helper performs this lookup; `resolve_agent_cli(provider_id)` maps the id to a CLI identifier (`claude`, `gemini`, `codex`, `openclaw`, `hermes`). Both are pure reads from loaded config.

Pure-Claude runs (no `config.agents` block at all) are byte-for-byte identical to pre-Spec-3 behavior.

### Dispatch primitives by CLI

The resolved `cli` field determines which execution path runs inside `dispatch_sigma_subagent()`:

| `cli` value | Dispatch primitive | Notes |
|-------------|-------------------|-------|
| `claude` | `Task(subagent_type=..., prompt=...)` | Skill owns the Task() call; context manager emits events only |
| `gemini` | subprocess via `dispatch_sigma_subagent` | Context manager owns the subprocess |
| `codex` | subprocess via `dispatch_sigma_subagent` | Prompt fed via stdin |
| `openclaw` | subprocess via `dispatch_sigma_subagent` | Uses `-p` flag |
| `hermes` | `NotImplementedError` (Spec 3 scope) | Route to another provider until hermes adapter ships |

### Event evidence requirement

`dispatch_sigma_subagent()` emits a paired `subagent_spawn` + `subagent_complete` event to `.fno/events.jsonl` for every subagent invocation. In mixed-provider runs (at least one non-Claude agent), the stop hook's `verify_event_evidence` path (Task 3.1, `ab-978e93ed`) validates these pairs before accepting the gate. Each pair must be on disk before `<promise>` is emitted.

For the run-time dispatch pseudocode see the **Per-Agent Provider Routing** section.
