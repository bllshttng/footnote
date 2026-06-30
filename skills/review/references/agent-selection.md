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

## Cross-Model Provider Routing

Panel agents can be routed to a different coding model (`codex` / `gemini`) via
`config.review.cross_model` / `config.review.agent_providers` in
`.fno/settings.yaml` - the SAME config the internal `fno review` panel honors,
resolved by the same `provider_resolution` path. `/review sigma` reads it through
`fno review --print-providers`, so the two surfaces never drift.

### Config schema

```yaml
config:
  review:
    cross_model:
      enabled: true        # correctness agents cross-model by default
    agent_providers:       # optional explicit pins (override the default)
      code_reviewer: codex
      silent_failure_hunter: gemini
```

Agent keys are the orchestrator's **underscore** names (`code_reviewer`,
`silent_failure_hunter`, `type_design_analyzer`, `ux_flow_tester`,
`multi_device_checker`, `integration_test_analyzer`) - the `_`<->`-` mapping of
the hyphenated `Task()` `subagent_type`. A key naming an unknown agent is warned
and ignored. When neither key is set, the panel is byte-for-byte today's
all-Claude run.

### Dispatch + degradation

Resolution and dispatch (claude -> `Task()`; codex/gemini -> `fno agents spawn
--once`; unavailable alternate -> graceful fallback to claude with a `reason`)
are owned by the **Cross-Model Review Routing** section in `sigma.md`. The
`dispatch_sigma_subagent()` helper (`cli/src/fno/sigma_dispatch.py`) may still
emit the forensic `subagent_spawn` / `subagent_complete` event pair for
non-Claude dispatches; it is forensics-only and does not affect the verdict.
