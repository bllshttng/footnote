
<!-- HEADLESS-SAFE INVARIANTS (enforced when invoked by megawalk's review-mode Driver)

This skill is invoked headlessly by megawalk's review-mode Driver invocation
(claude --print --max-turns 50 --dangerously-skip-permissions). All decisions
MUST be deterministic from the plan + diff alone. Do NOT add interactive prompts,
"ask the user" branches, or any step that requires human input mid-execution.

The six-agent panel dispatch (Task tool calls) is preserved and is exactly
what megawalk's review-mode exists to support.
-->

# Abilities Code Review

Review changes (local commits or PR) with focus on **integration tests** and **UX flow tests** - simulating how a human QA tester would manually test the feature.

**This skill is MANDATORY** - always invoke it. The skill internally detects change types and runs the appropriate agents. Callers do not need conditionals.

**Works with:**
- Local commits vs `origin/main` (no PR needed)
- Existing PR (optional - adds PR context)

## Philosophy

**We don't care about unit tests.** We care about:
- Does the integration actually work end-to-end?
- Does the UI behave correctly from a user's perspective?
- Does it work on different screen sizes?
- Would a human tester find bugs?

## Available Subagents

This skill orchestrates **specialized subagents** organized by concern:

### Quality Review (Code Analysis)

| Subagent | Model | Purpose |
|----------|-------|---------|
| `code-reviewer` | opus | CLAUDE.md compliance, bugs, code quality |
| `type-design-analyzer` | **sonnet** | Type invariants, encapsulation quality |

### Test Coverage

| Subagent | Model | Purpose |
|----------|-------|---------|
| `integration-test-analyzer` | inherit | Journey tests, DB verification |

### UX & Manual Testing (Human QA Simulation)

| Subagent | Model | Purpose |
|----------|-------|---------|
| `ux-flow-tester` | **sonnet** | User journeys, error states, UI updates |
| `multi-device-checker` | **sonnet** | Responsive design, touch targets |
| `silent-failure-hunter` | inherit | Swallowed errors, missing user feedback |

## Reference Materials

Load references during code review execution:

| Reference | Load When | Content |
|-----------|-----------|---------|
| [references/agent-selection.md](references/agent-selection.md) | Step 1: Detecting change types and selecting agents (ALWAYS load) | Change type detection script, conditional agent selection matrix, browser testing commands, automated check commands |
| [references/report-template.md](references/report-template.md) | Step 6: Generating the review report (ALWAYS load) | Structured report format with agents run/skipped, automated checks, verdict |

## Execution Process (MANDATORY)

When this skill is invoked, execute ALL steps in order. The skill decides what to run - callers should NOT add conditionals.

### Step 0.7: Load Execution Context from Scratchpad (AUTO)

Before starting the review, check for scratchpad execution data:

```bash
SCRATCHPAD=$(sed -n 's/^scratchpad_path:[[:space:]]*//p' .fno/target-state.md 2>/dev/null)
if [[ -n "$SCRATCHPAD" && -d "$SCRATCHPAD/execution" ]]; then
  # Read wave results and task results for execution context
  for result_file in "$SCRATCHPAD/execution"/*.md; do
    [[ -f "$result_file" ]] || continue
    # Parse task results, concerns, deviations
  done

  # Read plan summary for design constraints
  if [[ -f "$SCRATCHPAD/plan-summary.md" ]]; then
    # Constraints inform what the review should check
  fi
fi
```

If scratchpad data is available, aggregate it into `{scratchpad}/review-input.md`:

```bash
if [[ -n "$SCRATCHPAD" && -d "$SCRATCHPAD" ]]; then
  cat > "$SCRATCHPAD/review-input.md" << EOF
## Review Context (auto-generated)

## Execution Summary
[Aggregated from wave results - tasks completed, concerns raised]

## Design Constraints
[From plan-summary.md - things the review should verify]

## Files to Focus On
[Union of changed files across all waves]

## Known Concerns
[DONE_WITH_CONCERNS items from worker results]
EOF
fi
```

This context enriches the review but is not required. The review must still
function based on git diff + plan alone (backward compatibility).

### Step 1: Detect Change Type and Review Tier (MANDATORY)

Load [references/agent-selection.md](references/agent-selection.md) for:
1. Diff size tier detection (light/standard/full)
2. Change type detection (frontend/backend/full-stack/docs-only)
3. Agent selection based on both dimensions

### Step 2: Run Base Agents (MANDATORY - Always Run)

Always run `silent-failure-hunter` and `code-reviewer` regardless of change type.

### Step 3: Run Conditional Agents (Skill Decides)

Based on detected change type, add appropriate agents. See [references/agent-selection.md](references/agent-selection.md) for the full conditional logic.

### Step 3b: Confidence Scoring (MANDATORY for issues found)

For each issue flagged by agents in Steps 2-3, spawn a parallel Haiku validation agent:
- Input: issue description + relevant code snippet + CLAUDE.md context
- Agent scores 0-100 confidence using this rubric:
  - **0**: False positive, doesn't stand up to scrutiny, or pre-existing issue
  - **25**: Might be real, but couldn't verify. Stylistic issue not in CLAUDE.md
  - **50**: Real issue, but minor or unlikely in practice
  - **75**: Verified real issue, important, directly impacts functionality or violates CLAUDE.md
  - **100**: Confirmed definite issue, will happen frequently in practice

Filter out issues scoring below **80**. Only report high-confidence issues.

For CLAUDE.md-related issues: validator must verify the CLAUDE.md actually calls out that specific issue.

### Step 4: Run Automated Checks (MANDATORY)

Run typecheck, lint, journey tests, integration tests, and build. See [references/agent-selection.md](references/agent-selection.md) for commands.

#### Anti-Pattern Scan (MANDATORY)

Run the anti-pattern scanner on changed files:

```bash
bash scripts/scan-antipatterns.sh .
```

- ERROR-level findings (stubs, hardcoded secrets) are **blocking** — verdict must be `ready-to-merge` to proceed
- WARN-level findings (TODO/FIXME) are reported but non-blocking
- Include results under `## Anti-Pattern Scan` in the review report

### Step 5: Browser Testing (Conditional - Skill Decides)

**Only if frontend changes detected.** See [references/agent-selection.md](references/agent-selection.md) for browser testing commands.

### Step 5b: Eligibility Re-Check (if reviewing a PR)

Before generating the report, re-verify the PR is still eligible:
```bash
# Re-check PR state (may have changed during review)
gh pr view --json state,isDraft --jq '{state: .state, isDraft: .isDraft}'
```
- If PR is now closed/merged → skip posting, report locally only
- If PR is now a draft → skip posting, report locally only
- If Claude already commented → skip posting to avoid duplicates

### Step 6: Generate Report (MANDATORY)

Load [references/report-template.md](references/report-template.md) for the structured output format.

#### Goal Relevance (if settings.yaml has goals)

Read `project.goals` from settings.yaml (`.fno/settings.yaml` or `~/.fno/settings.yaml`). For each goal:
1. Determine if the changes are **Primary** (directly advance), **Secondary** (support), or **Not related**
2. If changes touch areas outside ALL stated goals, flag as potential scope creep
3. This is INFORMATIONAL — does not affect the PASS/FAIL verdict

### Step 6b: Plan-Drift Detection (when in target session)

If `.fno/target-state.md` exists and has `input_type: plan`:

1. Read the plan's 00-INDEX.md `## Files Modified` section
2. Parse expected files and their task attributions
3. Get actual changes: `git diff --name-only main...HEAD`
4. Compare:
   - Files in diff but NOT in plan → **DRIFT** warning
   - Files in plan but NOT in diff → **MISSING** warning
5. Report findings under `## Plan Drift Analysis`

These are WARNINGs — agents may have valid reasons to modify additional files.
Exclude common non-plan files: lock files, `.fno/*`, test fixtures, `node_modules`.

## What We DON'T Check

- Unit test coverage (we don't care)
- Code style beyond lint (auto-formatted)
- Implementation details (if tests pass, it works)

## What We DO Check

| Concern | Subagent | When |
|---------|----------|------|
| Silent failures | silent-failure-hunter | Always |
| CLAUDE.md compliance | code-reviewer | Always |
| User journeys | ux-flow-tester | Frontend |
| Responsive design | multi-device-checker | Frontend |
| Type invariants | type-design-analyzer | Backend |
| Integration tests | integration-test-analyzer | Backend |

## Key Principles

- **Mandatory invocation** - Call this skill; it decides what runs
- **Transparent reporting** - Always shows what ran and what was skipped
- **Test like a human would** - Click through flows, try bad inputs
- **Verify outcomes, not implementation** - Database state matters, code style doesn't
- **Multi-device is required** - Most users are on mobile (when frontend)
- **Integration > Unit** - If the integration works, the units work
- **Parallel review** - Subagents run simultaneously for speed
- **Ship with confidence** - Tests passing = safe to merge

## NEVER (Anti-Patterns)

**NEVER blindly trust subagent results:**
- Subagents run in isolated context — they don't see the full picture
- A subagent reporting "no issues" may have missed context you have
- Always cross-check critical findings against your own understanding

**NEVER skip the silent-failure-hunter:**
- It runs ALWAYS, regardless of change type
- If it returns empty results on code with try/catch blocks, that's suspicious — investigate
- Empty results ≠ no issues; it may mean the hunter's grep patterns didn't match

**NEVER report "all checks pass" without running them:**
- "Typecheck passed" means you ran `pnpm typecheck` and saw 0 errors in THIS message
- "Tests pass" means you ran the test command and counted 0 failures NOW
- Previous runs, agent claims, and "should pass" are not evidence

**NEVER let subagent disagreements go unresolved:**
- If code-reviewer says "good" but silent-failure-hunter flags an issue → investigate
- If ux-flow-tester says "works" but integration-test-analyzer finds gaps → reconcile
- Conflicts between agents are signals, not noise

**NEVER assume frontend-only changes have no backend impact:**
- Form submissions hit APIs
- Client-side validation doesn't replace server-side
- New UI states may need new error handling paths

<!--
  Per-agent provider routing schema and dispatch flow are documented in
  docs/provider-rotation.md#per-agent-routing-spec-3. The schema lives
  under config.agents.<name>.provider in .fno/settings.yaml.
-->

## After the review: what happens with findings

Post-review, verdicts inform the operator and the PR description. Deferred findings
(items the panel flagged but chose not to block on) go into the PR body or the plan's
COMPLETION.md so they surface to human reviewers rather than disappearing.

There is no gate artifact to write and no `fno gate` call to make. The review happened;
the six-agent panel output is the proof. The PR description carries the verdict forward.

## Per-Agent Provider Routing (optional)

Each subagent (`code-reviewer`, `silent-failure-hunter`, `type-design-analyzer`, etc.) can be pinned to a specific provider via `config.agents.<agent-name>.provider` in `.fno/settings.yaml`. If that key is unset, the agent uses the globally active provider - today's behavior, fully back-compatible.

Example `settings.yaml` block:

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

Agent names under `config.agents.<name>` MUST exactly match the `subagent_type` strings passed to `Task()` (case-sensitive). This is the same constraint as the existing `AGENTS_DISPATCHED` list - the same string is used in both places.

### Resolve provider before each Task() call

Before invoking each subagent, resolve its provider and CLI via two pure config helpers:

```python
provider_id = resolve_agent_provider(agent_name)  # config.agents.<name>.provider or global active
cli = resolve_agent_cli(provider_id)              # "claude" | "gemini" | "codex" | "openclaw" | "hermes"
```

Both functions are pure reads of the loaded config and can be called inline. `resolve_agent_provider` returns the pinned provider id when `config.agents.<name>.provider` is set; otherwise it returns the global active provider's id. `resolve_agent_cli` maps the provider id to its CLI identifier.

### Wrap each Task() call in dispatch_sigma_subagent()

The `dispatch_sigma_subagent()` context manager from `cli/src/fno/sigma_dispatch.py` handles spawn/complete event emission so the `verify_event_evidence` path in the stop hook (Task 3.1, `ab-978e93ed`) can confirm non-Claude subagent dispatches. Use it for EVERY subagent invocation:

```python
# For each subagent in AGENTS_DISPATCHED:
provider_id = resolve_agent_provider(agent_name)  # from config.agents or global active
cli = resolve_agent_cli(provider_id)              # claude | gemini | codex | openclaw | hermes

with dispatch_sigma_subagent(
    agent_name=agent_name,
    provider_id=provider_id,
    cli=cli,
    prompt=...,
) as dispatch:
    if cli == "claude":
        # For Claude: the skill owns the Task() invocation.
        # dispatch_sigma_subagent only emits the paired events.
        result = Task(subagent_type=agent_name, prompt=...)
        dispatch.record_complete(stdout=result.stdout, exit_code=0)
    else:
        # For non-Claude paths (gemini, codex, openclaw):
        # dispatch_sigma_subagent owns the subprocess.
        # The context manager exits with the result captured in dispatch.result.
        # hermes is currently NotImplementedError - route to an available provider.
        pass  # control flow handled inside the context manager
    # After exit, paired subagent_spawn + subagent_complete events are
    # on disk in .fno/events.jsonl.
```

### Finding Attribution: provider_id on each finding

When merging findings from the dispatched agents into the final review report, each
finding object MUST carry a `provider_id` field set to the dispatching agent's resolved
provider - the same string returned by `resolve_agent_provider(agent_name)` and passed
to `dispatch_sigma_subagent(provider_id=...)`. Adjacent to the existing `agent` field:

```yaml
- file: path/to/foo.py
  line: 42
  severity: high
  message: "swallowed exception hides DB write failure"
  agent: silent-failure-hunter
  provider_id: gemini-pro-1
```

This field is forensics-only. It does NOT affect verdict logic - a HIGH finding from
any provider is still a HIGH finding and triggers the same blocking behavior. Severity
remains the gate-passing input. The `provider_id` is for post-mortem analysis:
future audit_loop iterations can use it to detect patterns like "all my Gemini findings
disappeared between iterations - did Gemini go down?" without re-running the review.

When `config.agents.<name>.provider` is unset (pure-Claude run), `provider_id` is the
global active provider's id (e.g., `"claude-anthropic"`). It is never empty or absent.

### Pure-Claude vs mixed-provider behavior

**Pure-Claude runs** (no `config.agents.<name>` set for any agent): dispatch events are emitted for forensics. Behavior is byte-for-byte identical to pre-Spec-3 runs.

**Mixed runs** (at least one agent pinned to a non-Claude provider): each `subagent_spawn` + `subagent_complete` pair lands on disk in `.fno/events.jsonl` for forensic correlation. See `references/agent-selection.md` for the config schema.

