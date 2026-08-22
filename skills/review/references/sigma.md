
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

### Step 0: Read the resolved route (MANDATORY, before any dispatch)

The panel is six subagents wide. A shared-quota account cannot afford that, and one already spent eight subagents on a single review that returned nothing. So read the route this session was given before you dispatch anything:

```bash
fno whoami | grep '^provider:' || true
```

A `subagent budget` below 2 on that line means the panel is not the route here. Print the provider and the budget. Then run ONE inline reviewer instead. Read the changed files yourself on this thread and report against the same checklist. Do not dispatch the six agents, do not dispatch a smaller panel, and do not queue them sequentially. The budget is about what the account spends, not about how fast it spends it.

No line at all means this session carries no route stamp. That is the operator's own account, so the panel runs exactly as it always has.

This step READS the route. It never decides it. The decision lives in `fno.review_capability`. That resolver marks `sigma` unavailable under a budget below 2 and names `code-review` as the resolved route. Prose here that merely preferred one reviewer is the decorative form of the fix. A session can ignore prose and fan out anyway. Printing the resolved route is what makes an explicit `/fno:review sigma` visible rather than silent.

### Step 1: Detect Change Type and Review Tier (MANDATORY)

Load [agent-selection.md](agent-selection.md) for:
1. Diff size tier detection (light/standard/full)
2. Change type detection (frontend/backend/full-stack/docs-only)
3. Agent selection based on both dimensions

Before dispatch, pin the revision that every reviewer and the durable artifact describe:

```bash
REVIEWED_HEAD=$(git rev-parse HEAD) || exit 1
```

Do not resolve this value after the panel runs.
If the head advances during review, Step 6d retains the completed round without replacing the current alias.

### Step 1b: Resolve Review Scope (MANDATORY - the single changed-files producer)

Every diff-derived "which files changed" read in this pass resolves from this step. Tier detection, change-type detection, dispatch prompts, and plan-drift detection all consume it. Never compute a second changed-files list anywhere else in the pass. A second producer scanning a different base reports narrow but scans wide, or the reverse. That is a silent coverage lie. The static checks stay project-wide by design: typecheck, lint, build, and the anti-pattern scan are not diff-derived. The report names them as unscoped so a reader can tell.

The scope narrows only to the increment since the last reviewed head, read from the durable artifact via the read-only accessor. Full scope is the fail-open default: any doubt about the prior head, the artifact, or the rules produces MORE review, never less.

```bash
MERGE_BASE=$(git merge-base origin/main HEAD 2>/dev/null || echo origin/main)
SCOPE_BASE="$MERGE_BASE"
SCOPE_REASON="first-round"

NODE_ID=$(sed -n 's/^graph_node_id:[[:space:]]*//p' .fno/target-state.md 2>/dev/null | head -1 | xargs)
PR_NUMBER=$(fno do pr info 2>/dev/null | jq -r '.pr // empty')
PRIOR_HEAD=""
if [ -n "$NODE_ID" ] && [ -n "$PR_NUMBER" ]; then
  PRIOR_HEAD=$(fno do review --sigma-last-head --sigma-node "$NODE_ID" --sigma-pr "$PR_NUMBER" 2>/dev/null || true)
fi

if [ -n "$PRIOR_HEAD" ]; then
  if git merge-base --is-ancestor "$PRIOR_HEAD" HEAD 2>/dev/null; then
    if git diff --name-only "$PRIOR_HEAD..HEAD" | grep -qE '(^|/)(CLAUDE\.md|AGENTS\.md)$|^\.claude/rules/'; then
      SCOPE_REASON="rules-changed"    # a new rule can condemn cleared code: full scope
    else
      SCOPE_BASE="$PRIOR_HEAD"
      SCOPE_REASON="incremental"
    fi
  else
    SCOPE_REASON="history-rewritten"  # rebase / squash / force-push / GC: full scope
  fi
fi

CHANGED_FILES=$(git diff --name-only "$SCOPE_BASE..HEAD")
FULL_DIFF_FILES=$(git diff --name-only "$MERGE_BASE..HEAD")

# An empty increment (empty commit) must never reach dispatch as a zero-file
# vacuous pass. Reset to the fail-open full scope instead.
if [ "$SCOPE_REASON" = "incremental" ] && [ -z "$CHANGED_FILES" ]; then
  SCOPE_BASE="$MERGE_BASE"
  SCOPE_REASON="first-round"
  CHANGED_FILES="$FULL_DIFF_FILES"
fi
```

Fallback semantics, each mapping to full scope:

| Condition | `SCOPE_REASON` | Why full |
|---|---|---|
| No artifact, no PR, accessor error | `first-round` | Nothing was ever reviewed here |
| Prior head not an ancestor of HEAD | `history-rewritten` | The old head no longer names reachable state |
| Increment touches `CLAUDE.md`, `AGENTS.md`, or `.claude/rules/` (at any depth) | `rules-changed` | A new rule can condemn already-cleared code |
| Increment is empty | resets to `first-round` | A zero-file dispatch passes vacuously |

`PRIOR_HEAD` is a snapshot from an artifact, not proof the commit is still reachable. The `git merge-base --is-ancestor` check is the verification, and it is not optional. A non-zero exit from `fno do review --sigma-last-head` is a normal, expected input meaning "review everything". It must never raise into the pass.

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

### Step 3c: Carry Forward Unresolved Prior Findings (MANDATORY on incremental rounds)

On every `incremental` round, run this step between the panel results and the verdict. Today the full re-review is the resolution mechanism: the fresh full-diff findings re-derive every prior blocker. An unfixed one resurfaces on its own. Narrowing the scope removes that mechanism. Without a replacement, an incremental round over a one-file increment reports zero findings while round 1's blocker is still live. The gate then clears over an unfixed defect. This step is the replacement. Scope narrowing is unsafe to ship without it.

When its cited quote still validates at the current head, a prior blocking finding is unaddressed. That is the checkable definition of unaddressed.

1. Read the prior round's report. `PRIOR_HEAD` satisfies the inspect validator's expected head by construction, so the existing read surface returns the body. Keep the exit status: it decides what an empty result means.

   ```bash
   if PRIOR_REPORT=$(fno do review --inspect-sigma --sigma-node "$NODE_ID" --sigma-pr "$PR_NUMBER" \
     --sigma-head "$PRIOR_HEAD" --json 2>/dev/null); then
     INSPECT=ok
   else
     INSPECT=failed
   fi
   ```

   With `INSPECT=ok` and no critical or high findings in the body, there is nothing to carry. The round proceeds on its own findings alone.

   With `INSPECT=failed`, the prior report was not readable. The narrowed scope is unproven, so a live blocking finding can sit in the unread report. Never read a failed inspection as an empty prior report. Re-run this round at full scope (`SCOPE_BASE=$MERGE_BASE`) before any verdict. No attestation can be emitted from the incomplete round.

2. For each **critical** or **high** finding in the prior body, spawn the same Haiku cite-or-drop validator as Step 3b. Run it against the CURRENT head. Those two severities are the ones that block.
   - quote still matches at the cited `file:line` -> the finding is **unresolved**. Carry it verbatim into this round's report and verdict. Tag it `carried from round <id>` under Critical/High Issues.
   - quote is gone or no longer supports the claim -> it lands in the 0-25 abstain band. It falls below 80 and drops out. The same rubric that decided admission decides resolution. No separate "addressed" flag and no cached verdict exist.

3. A carried finding counts as a blocking finding of THIS round. While any carried finding is unresolved, the verdict cannot be `ready-to-merge`. Step 6c emits no attestation over it.

Every round's report is therefore the union of findings newly derived from the incremental scope and carried findings that still validate at the current head. The cost is one Haiku validation per prior blocking finding, which is far below re-running the panel over the whole diff.

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
PR_NUMBER=$(fno do pr info | jq -er .pr)
gh api "repos/{owner}/{repo}/pulls/$PR_NUMBER" --jq '{state: (.state | ascii_upcase), isDraft: .draft}'
```
- If PR is now closed/merged → skip posting, report locally only
- If PR is now a draft → skip posting, report locally only

Do not use the existence of any prior Claude-authored comment as a dedup signal.
After the report is durable, deduplicate only on the explicit marker for this reviewed head and round.

### Step 6: Generate Report (MANDATORY)

Load [report-template.md](report-template.md) for the structured output format.
Render the complete report once to a temporary file as well as to the user-facing response; this exact file is the input to the shared artifact writer in Step 6d.

The report header carries the **Review Scope** line from Step 1b: `$SCOPE_REASON`, plus `$SCOPE_BASE` and the changed-file count on an incremental round. A narrowed round that reads as full coverage is a silent coverage lie.

#### Goal Relevance (if config.toml has goals)

Read `project.goals` from config.toml (`.fno/config.toml` or `~/.fno/config.toml`). For each goal:
1. Determine if the changes are **Primary** (directly advance), **Secondary** (support), or **Not related**
2. If changes touch areas outside ALL stated goals, flag as potential scope creep
3. This is INFORMATIONAL — does not affect the PASS/FAIL verdict

### Step 6b: Plan-Drift Detection (when in target session)

If `.fno/target-state.md` exists and has `input_type: plan`:

1. Read the plan's 00-INDEX.md `## Files Modified` section
2. Parse expected files and their task attributions
3. Get actual changes: `$FULL_DIFF_FILES` from the Step 1b producer. Never run a second `git diff` against a possibly-stale local `main`
4. Compare:
   - Files in diff but NOT in plan → **DRIFT** warning
   - Files in plan but NOT in diff → **MISSING** warning
5. Report findings under `## Plan Drift Analysis`

These are WARNINGs — agents may have valid reasons to modify additional files.
Exclude common non-plan files: lock files, `.fno/*`, test fixtures, `node_modules`.

### Step 6c: Emit the reviewers-gate attestation (only on a clean PASS)

Emit the head-pinned `review_attestation` only on a clean PASS: the verdict is `ready-to-merge` with no unaddressed blocking finding after Steps 3b/3c/4. A `config.review.reviewers: [sigma]` gate then clears:

```bash
bash "${SKILL_DIR}/scripts/emit-attestation.sh" sigma
```

This is what lets a solo / claude-only harness (no GitHub App bot) express a real, auditable review gate. Rules:
- **Never emit on a blocking finding:** a failing or blocked panel emits nothing. Absence holds the gate. That is fail closed. A carried-forward finding that still validates at the current head (Step 3c) is a blocking finding of this round.
- **Head-pinned, and cumulative in meaning.** The helper stamps the current HEAD. If new commits land after this pass, re-run sigma. The old attestation no longer counts. Loop-check discards a `head_sha` that is not the current HEAD. An attestation asserts coverage **cumulative across rounds up to this head**. Every file in the diff was reviewed in some round at or before this head. Every prior blocking finding was re-validated at this head by Step 3c. It does NOT assert that the full diff was re-reviewed in one pass.
- **Advisory without a gate:** with no `reviewers` entry naming `sigma`, the event is harmless telemetry. Loop-check reads it only under a configured gate.

### Step 6d: Persist the report before deciding whether to comment

Every completed panel writes the node-bound artifact, including draft, closed, merged, duplicate-comment, and comment-failure paths.
Resolve the reviewed head before dispatch and retain it as `REVIEWED_HEAD`; resolve the current head again immediately before publication so a late review of an older head is retained under `rounds/` without displacing `sigma.md`.

```bash
NODE_ID=$(sed -n 's/^graph_node_id:[[:space:]]*//p' .fno/target-state.md | head -1 | xargs)
PR_INFO=$(fno do pr info)
PR_NUMBER=$(printf '%s' "$PR_INFO" | jq -er .pr)
CURRENT_HEAD=$(printf '%s' "$PR_INFO" | jq -er .head_sha)
ROUND_ID="${REVIEWED_HEAD:0:12}-$(date -u +%Y%m%dT%H%M%SZ)-$$"
fno do review --publish-sigma "$REPORT_FILE" \
  --sigma-node "$NODE_ID" --sigma-pr "$PR_NUMBER" \
  --sigma-head "$REVIEWED_HEAD" --sigma-current-head "$CURRENT_HEAD" \
  --sigma-round "$ROUND_ID" \
  --sigma-scope-base "$SCOPE_BASE" --sigma-scope-reason "$SCOPE_REASON"
```

The scope pair is what makes a later round trust this artifact's head as a narrowing base. An artifact without it is treated as no prior head. Omit these two flags and the next round reviews the full diff again. That is fail-open by construction.

Treat a publication error as a failed review handoff and surface it; never claim the report is durable.
The command prints the primary path, reviewed head, and whether compare-and-publish accepted it.

### Step 6e: Project the durable report to the PR when eligible

For an open, non-draft PR, search issue comments for `<!-- fno-sigma head=$REVIEWED_HEAD round=$ROUND_ID -->`.
Post exactly one comment containing the report and that marker when it is absent; a retry with the same marker posts nothing, while a new head or explicit new round is not suppressed by an older local-user comment.
The artifact remains authoritative if this comment call fails.

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
  Cross-model review routing (config.review.agent_routes / legacy agent_harnesses) is
  documented in the "Cross-Model Review Routing" section below. It is resolved
  by `fno do review --print-providers`, the SAME resolver the `fno do review` panel
  uses, so /review sigma and fno do review never drift.
-->

## After the review: what happens with findings

Post-review, verdicts inform the operator and the PR description. Deferred findings
(items the panel flagged but chose not to block on) go into the PR body or the plan's
COMPLETION.md so they surface to human reviewers rather than disappearing.

The durable node-bound sigma artifact is the primary carrier; the PR comment is its human-facing projection and the attestation is separate gate evidence.
The PR-owning `/fno:pr check` session consumes the artifact and owns all implementation decisions.

**When the approach is unsalvageable** - wrong architecture, a cascading design error,
patch-on-patch accumulation where each fix spawns the next - the panel may emit the
terminal `RECOMMEND RESTART` verdict instead of a fix-in-place blocking review. It is
legal only with a why-fix-in-place-fails rationale and a lessons block; severity alone
never triggers it. Contract: [report-template.md](report-template.md), "Terminal
recommendation: RECOMMEND RESTART". Honor sequence:
`skills/target/references/failure-recovery.md`, "Reviewer-ordered restart".

## Cross-Model Review Routing (optional)

By default every panel agent runs through `Task()` on the invoking harness.
An operator can route specific agents through a full harness, route-provider, and model tuple for a genuine cross-model read by setting `config.review.agent_routes`.
Legacy `config.review.cross_model` and `config.review.agent_harnesses` remain supported.
Configure them in `.fno/config.toml`, which is the same config the internal `fno do review` panel honors.
When none of these routing options is set, this whole section is a no-op and the panel is byte-for-byte today's harness-local run.

```yaml
config:
  review:
    cross_model:
      enabled: true        # turn the correctness agents cross-model by default
    agent_routes:          # explicit opt-in; each entry creates one named session
      code_reviewer:
        harness: claude
        provider: zai
        model: glm-5.2
```

Agent names are the orchestrator's underscore form (`code_reviewer`, `silent_failure_hunter`, `type_design_analyzer`, `integration_test_analyzer`, `ux_flow_tester`, `multi_device_checker`), not the hyphenated `Task()` `subagent_type` (`code-reviewer`).
The mapping is just `_`<->`-`.

### Step R1: resolve routing (do NOT reimplement it)

Before dispatching the panel, ask the CLI for the per-agent routing.
This is the one resolver, using the same `provider_resolution` path that `fno do review` dispatches through, so `/review sigma` and `fno do review` never disagree:

```bash
# --session-id is optional; pass it when running inside a target session so the
# implementer-provider (cross-model excludes it) is accurate.
# Branch rather than build an argv array. An unquoted ${VAR:+...} is bash-only
# (zsh passes flag+value as ONE argument), and no array form is portable either:
# plain "${a[@]}" errors under bash set -u, and the guarded "${a[@]+...}" form
# passes one EMPTY argument under zsh.
if [ -n "${SESSION_ID:-}" ]; then
  ROUTING="$(fno do review --print-providers --session-id "$SESSION_ID")"
else
  ROUTING="$(fno do review --print-providers)"
fi
INVOKING_HARNESS="$(fno whoami 2>/dev/null | sed -n 's/^harness:[[:space:]]*//p' | head -1 | xargs)"
[ -n "$INVOKING_HARNESS" ] || INVOKING_HARNESS=unknown
```

`$ROUTING` includes runtime harness, route provider, effective model, degraded state, and reason, or `{}` when all routing features are off.
`INVOKING_HARNESS` is the observed runtime for a `Task()` dispatch; a requested provider is not execution evidence.

### Step R2: dispatch each agent by its resolved provider

For each panel agent, keep the requested route from `$ROUTING` separate from the observed runtime that returns the result:

- **routing disabled (`$ROUTING` is `{}`)** -> dispatch via `Task(subagent_type="<agent-hyphen>", prompt=...)` exactly as today and record `INVOKING_HARNESS` as the observed runtime.
  This is the only path the headless megawalk Driver ever takes because megawalk does not set cross-model, so the HEADLESS-SAFE invariant holds.
- **legacy provider equals `INVOKING_HARNESS`** -> dispatch via `Task()` and record `INVOKING_HARNESS`, never the resolver label, as the observed runtime.
- **legacy provider differs from `INVOKING_HARNESS`** -> run the synchronous one-shot spawn below with `--harness "$PROVIDER"`; a harness-local `Task()` cannot satisfy a cross-harness route.
- **explicit route tuple** -> dispatch a plugin-qualified named headless session through the existing spawn surface and preserve the strict JSON response contract:

  ```bash
  fno agents spawn --harness "$HARNESS" --substrate headless \
    --agent "fno:$AGENT_HYPHEN" --route "$ROUTE_PROVIDER/$MODEL" \
    -t 600 "$(cat "$BRIEF")"
  ```

  Judge the routed result by the same exit-code and non-empty-output contract as the legacy one-shot branch.
  On a non-zero exit, empty output, or unavailable route, fall back to `Task()` on the invoking harness for that agent and record `INVOKING_HARNESS` plus its effective model when the runtime exposes one; otherwise record `unknown` rather than falsely retaining the requested routed model.

- **cross-harness legacy provider (`claude` / `codex` / `gemini`)** -> run a synchronous one-shot through the same lane `/review peer` uses.
  Write the agent's review brief to a shell-safe file, then run it so the reply returns in-context; never ask the user to run it.

  ```bash
  if [ "$PROVIDER" = "claude" ]; then
    fno agents spawn --harness claude --substrate headless \
      --agent "fno:$AGENT_HYPHEN" -t 300 "$(cat "$BRIEF")"
  else
    fno agents spawn --harness "$PROVIDER" --once \
      -t 300 --name "sigma-$AGENT" "$(cat "$BRIEF")"
  fi
  ```

  Judge by exit code and emptiness only: exit 0 plus non-empty stdout folds those findings into the report.
  A non-zero exit, empty output, unavailable daemon, or missing binary falls back to `Task()` on the invoking harness for that agent and records the fallback.
  Never fabricate findings to fill the gap.

- **`degraded: true`** -> use the resolved provider under the same rules and surface `reason` in the report so the run cannot silently appear cross-modeled.

If the resolved provider differs from `INVOKING_HARNESS`, a `Task()` dispatch is a route violation rather than a successful cross-model run.
Never copy the requested provider into the observed runtime.
On fallback, record the requested route, the actual invoking harness, `unknown` when no model receipt exists, and a degraded reason.

### Finding attribution

Every per-agent row records the requested route, observed runtime harness, and effective model, including an `unknown` marker when the runtime exposes no model receipt.
The requested route comes from resolution; the observed runtime comes only from the dispatch mechanism or its receipt.
When a finding comes from a cross-modeled agent, tag it with the dispatching provider and effective model from runtime dispatch evidence next to the existing `agent` field.
This is forensics-only: a HIGH finding is HIGH regardless of provider and triggers the same blocking behavior.

Each explicit tuple creates a separate SessionStart preamble.
At the measured 50–60K tokens per start, explicitly routing all six agents costs roughly 300–360K preamble tokens; keep this opt-in and prefer whole-session routing when every agent should use the same model.
A legacy cross-harness spawn has the same per-agent preamble cost: the default three-agent correctness subset costs roughly 150–180K tokens, while all six cost roughly 300–360K.

### Quick cross-model second opinion

This routing cross-models the *panel*.
For a fast one-shot read of a whole diff from another model without running the six-agent panel, use `/review peer [PR#|branch] [codex|gemini]` instead; it is advisory and never satisfies a `required_bots` gate.
