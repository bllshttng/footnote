
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
| [agent-selection.md](agent-selection.md) | Step 1: Detecting change types and selecting agents (ALWAYS load) | Change type detection script, conditional agent selection matrix, browser testing commands, automated check commands |
| [report-template.md](report-template.md) | Step 6: Generating the review report (ALWAYS load) | Structured report format with agents run/skipped, automated checks, verdict |

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

Load [agent-selection.md](agent-selection.md) for:
1. Diff size tier detection (light/standard/full)
2. Change type detection (frontend/backend/full-stack/docs-only)
3. Agent selection based on both dimensions

### Step 2: Run Base Agents (MANDATORY - Always Run)

Always run `silent-failure-hunter` and `code-reviewer` regardless of change type.

### Step 3: Run Conditional Agents (Skill Decides)

Based on detected change type, add appropriate agents. See [agent-selection.md](agent-selection.md) for the full conditional logic.

### Step 3b: Confidence Scoring (MANDATORY for issues found)

For each issue flagged by agents in Steps 2-3, spawn a parallel Haiku validation agent:
- Input: issue description + the finding's cited `file:line` quote + relevant code snippet + CLAUDE.md context
- **Quote validation (cite-or-drop):** the validator MUST open the finding's cited `file:line` and check the finding's verbatim quote against the actual file content, not plausibility alone. A finding whose quote is missing from the cited location, does not match the file, or does not support the claim scores in the **0-25 abstain band** — it is treated as unverifiable, not asserted.
- Agent scores 0-100 confidence using this rubric:
  - **0**: False positive, doesn't stand up to scrutiny, or pre-existing issue
  - **25**: Might be real, but couldn't verify against the actual file content (abstain). Uncitable/unsupported quote. Stylistic issue not in CLAUDE.md
  - **50**: Real issue, but minor or unlikely in practice
  - **75**: Verified real issue, important, directly impacts functionality or violates CLAUDE.md
  - **100**: Confirmed definite issue, will happen frequently in practice

When uncertain whether the quote supports the claim, prefer the abstain band (0-25) over guessing high — a dropped uncertain finding is correct; a confidently wrong one is not.

Filter out issues scoring below **80**. Only report high-confidence issues. (The sub-80 threshold is unchanged; abstain-band findings simply fall below it.)

For CLAUDE.md-related issues: validator must verify the CLAUDE.md actually calls out that specific issue.

### Step 4: Run Automated Checks (MANDATORY)

Run typecheck, lint, journey tests, integration tests, and build. See [agent-selection.md](agent-selection.md) for commands.

#### Anti-Pattern Scan (MANDATORY)

Run the anti-pattern scanner on changed files:

```bash
bash scripts/scan-antipatterns.sh .
```

- ERROR-level findings (stubs, hardcoded secrets) are **blocking** — verdict must be `ready-to-merge` to proceed
- WARN-level findings (TODO/FIXME) are reported but non-blocking
- Include results under `## Anti-Pattern Scan` in the review report

### Step 5: Browser Testing (Conditional - Skill Decides)

**Only if frontend changes detected.** See [agent-selection.md](agent-selection.md) for browser testing commands.

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

Load [report-template.md](report-template.md) for the structured output format.

#### Goal Relevance (if config.toml has goals)

Read `project.goals` from config.toml (`.fno/config.toml` or `~/.fno/config.toml`). For each goal:
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

### Step 6c: Emit the reviewers-gate attestation (only on a clean PASS)

If — and only if — the verdict is `ready-to-merge` (no unaddressed blocking finding after Step 3b/Step 4), emit the head-pinned `review_attestation` so a `config.review.reviewers: [sigma]` gate can clear:

```bash
bash "${SKILL_DIR}/scripts/emit-attestation.sh" sigma
```

This is what lets a solo / claude-only harness (no GitHub App bot) express a real, auditable review gate. Rules:
- **Never emit on a blocking finding.** A failing or blocked panel emits nothing; absence holds the gate (fail closed).
- **Head-pinned.** The helper stamps the current HEAD. If new commits land after this pass, re-run sigma — the old attestation no longer counts (loop-check discards a `head_sha` that is not the current HEAD).
- **Advisory when not gating.** If no `reviewers` entry names `sigma`, the event is harmless telemetry; loop-check only reads it when the gate is configured.

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
  Cross-model review routing (config.review.cross_model / agent_providers) is
  documented in the "Cross-Model Review Routing" section below. It is resolved
  by `fno review --print-providers`, the SAME resolver the `fno review` panel
  uses, so /review sigma and fno review never drift.
-->

## After the review: what happens with findings

Post-review, verdicts inform the operator and the PR description. Deferred findings
(items the panel flagged but chose not to block on) go into the PR body or the plan's
COMPLETION.md so they surface to human reviewers rather than disappearing.

There is no gate artifact to write and no `fno gate` call to make. The review happened;
the six-agent panel output is the proof. The PR description carries the verdict forward.

**When the approach is unsalvageable** - wrong architecture, a cascading design error,
patch-on-patch accumulation where each fix spawns the next - the panel may emit the
terminal `RECOMMEND RESTART` verdict instead of a fix-in-place blocking review. It is
legal only with a why-fix-in-place-fails rationale and a lessons block; severity alone
never triggers it. Contract: [report-template.md](report-template.md), "Terminal
recommendation: RECOMMEND RESTART". Honor sequence:
`skills/target/references/failure-recovery.md`, "Reviewer-ordered restart".

## Cross-Model Review Routing (optional)

By default every panel agent runs on Claude (via `Task()`). An operator can route
specific agents to a different coding model (`codex` / `gemini`) for a genuine
cross-model read by setting `config.review.cross_model` / `config.review.agent_providers`
in `.fno/config.toml` - the SAME config the internal `fno review` panel honors.
When neither is set, this whole section is a no-op and the panel is byte-for-byte
today's all-Claude run.

```yaml
config:
  review:
    cross_model:
      enabled: true        # turn the correctness agents cross-model by default
    agent_providers:       # optional explicit pins (override the default)
      code_reviewer: codex
      silent_failure_hunter: gemini
```

Agent names are the orchestrator's underscore form (`code_reviewer`,
`silent_failure_hunter`, `type_design_analyzer`, `integration_test_analyzer`,
`ux_flow_tester`, `multi_device_checker`) - NOT the hyphenated `Task()`
`subagent_type` (`code-reviewer`). The mapping is just `_`<->`-`.

### Step R1: resolve routing (do NOT reimplement it)

Before dispatching the panel, ask the CLI for the per-agent routing. This is the
ONE resolver - the same `provider_resolution` path `fno review` dispatches through -
so `/review sigma` and `fno review` never disagree:

```bash
# --session-id is optional; pass it when running inside a target session so the
# implementer-provider (cross-model excludes it) is accurate.
# An unquoted ${VAR:+...} is bash-only: zsh passes the flag and its value as a
single argument. The array form behaves identically under both shells.
SID_ARG=(); [[ -n "${SESSION_ID:-}" ]] && SID_ARG=(--session-id "$SESSION_ID")
ROUTING="$(fno review --print-providers "${SID_ARG[@]+"${SID_ARG[@]}"}")"
```

`$ROUTING` is JSON `{ "<agent_underscore>": {"provider": "claude|codex|gemini",
"degraded": bool, "reason": str|null}, ... }`, or `{}` when cross-model is OFF.

### Step R2: dispatch each agent by its resolved provider

For each panel agent, read `provider` from `$ROUTING` (default `claude` when the
key is absent or `$ROUTING` is `{}`):

- **`claude`** -> dispatch via `Task(subagent_type="<agent-hyphen>", prompt=...)`
  exactly as today. This is the only path the headless megawalk Driver ever takes
  (megawalk does not set cross-model), so the HEADLESS-SAFE invariant holds.
- **`codex` / `gemini`** -> run a synchronous one-shot, the SAME lane `/review peer`
  uses. Write the agent's review brief to a file (shell-safe), then YOU run it
  (never the user) so the reply returns in-context:

  ```bash
  fno agents spawn --provider "$PROVIDER" --once -t 300 "sigma-$AGENT" "$(cat "$BRIEF")"
  ```

  Judge by exit code + emptiness only: exit 0 + non-empty stdout -> fold those
  findings into the report; **non-zero or empty (or the daemon/binary is missing)
  -> fall back to `Task()` on Claude** for that agent and note the fallback. NEVER
  fabricate findings to fill the gap.

- **`degraded: true`** (the resolver already returned `provider: claude` because no
  alternate was available) -> dispatch on Claude and surface `reason` in the report
  so the run reads as "cross-model unavailable: ran on claude" rather than silently
  appearing cross-modeled.

### Finding attribution

When a finding comes from a cross-modeled agent, tag it with the dispatching
`provider` (from `$ROUTING`) next to the existing `agent` field. This is
forensics-only: a HIGH finding is HIGH regardless of provider and triggers the
same blocking behavior. The forensic `subagent_spawn` / `subagent_complete` event
pair (via `dispatch_sigma_subagent` in `cli/src/fno/sigma_dispatch.py`) may still
be emitted for non-Claude dispatches; it does not affect the verdict.

### Quick cross-model second opinion

This routing cross-models the *panel*. For a fast one-shot read of a whole diff
from another model without running the six-agent panel, use
`/review peer [PR#|branch] [codex|gemini]` instead - it is advisory and never
satisfies a `required_bots` gate.

