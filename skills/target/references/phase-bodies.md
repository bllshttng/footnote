# Phase Bodies

**Load when:** about to execute the clean, review, or direction-alignment phase. Each phase has a small body of rules around when it runs and what counts as success.

## Clean Phase (3.5) - De-Sloppify

Optional phase that removes common AI code slop. Runs between Execute and Review.

**`no_clean` starts as `true` (clean is opt-in).** Changed only by:
1. CLI: `clean` positional modifier sets `no_clean: false`
2. Config: `no_clean: false` in config.toml

If `no_clean` is `true`: skip to Review phase.

If `no_clean` is `false`:
1. Invoke `/simplify` via the Skill tool
2. `/simplify` reviews all changed files (from git diff) for:
   - Over-defensive error handling (try/catch around infallible code)
   - Tests that test language features instead of business logic
   - Unnecessary type assertions and redundant type guards
   - Premature abstractions (helpers used exactly once)
   - Console.log/debugger statements left in
   - Comments that restate the code
3. `/simplify` makes the fixes directly (Edit tool)
4. Run build/tests to verify cleanup didn't break anything
5. If tests break: revert the cleanup changes, skip this phase, continue to Review

This phase runs as a **separate context** from Execute to avoid negative-instruction interference. The Execute phase says "build this." The Clean phase says "remove the bad parts." Mixing these in the same prompt degrades both.

## Review Phase (4) - Deferred Gate

The review phase invokes `fno:review`, which spawns subagents that may run in background. The pipeline does NOT block on review - ship and external review can proceed in parallel.

The review result is deferred: do NOT emit `<promise>` while sigma-review agents are still running. Wait until:

1. All sigma-review agents have returned results (check task notifications)
2. Critical and High findings are addressed (fixed or verified as false positives)
3. The review report verdict is "Ready to merge"

Do not assess the code yourself in lieu of waiting - the agents exist precisely to catch what you missed.

If both sigma-review agents AND external review (`/pr check`) flag the same issue, that's confirmation - fix it once, both gates benefit.

While the PR's CI is still polling, read posted optional bot reviews at first-post rather than deferring every read to green - the first-post review watch and its once-at-green backstop are specified in the "Watch for posted optional reviews" / "Drain a posted optional review" paragraphs of [SKILL.md](../SKILL.md). Same story on both surfaces: a real finding folds into the fix round in flight instead of adding a post-green round.

**Skip this pre-ship run when `config.review.reviewers` includes `sigma`.** In that case sigma is a *configured gate*, not advisory, and runs exactly once post-ship on the final shipped HEAD (see the Completion section of SKILL.md), where it emits the head-pinned attestation loop-check requires. Running it here too would be a wasted second panel whose attestation any later fix invalidates. When `sigma` is NOT a configured reviewer, run it here as the cheap advisory insurance as usual.

## Intent verification (no promise-time self-grade)

There is no promise-time self-grade phase (control-plane step 6, ab-f8e5f214).
Intent is checked through CI, not an agent grading its own homework: the BDD
acceptance criteria that matter become tests during `/do` (TDD) and run in CI,
which `fno-agents loop-check`'s CI read already covers. A genuinely missing test
is a `/blueprint`-quality problem, not a completion gate. There is no self-grade
PASS/FAIL line and no phase 4.5.

**Do phase mode dispatch:** Main thread uses `fno:do waves` directly. Subagent mode dispatches via Task tool per wave. Worktree mode creates separate worktrees per plan (see [multi-plan.md](multi-plan.md)).

## Direction Alignment Check (every 2 phases)

After completing an execution phase (do, review, validate - not think or plan), increment `alignment.phases_since_check` in target-state.md.

When `phases_since_check >= check_interval` (default: 2):

1. Read the plan file (`plan_path` from target-state.md)
2. Extract task list with descriptions and file ownership
3. Run: `git log --oneline --since="{created_at from target-state.md}"`
4. Determine tasks scheduled through the current wave (from 00-INDEX.md execution strategy)
5. For each scheduled task, check if a corresponding commit exists:
   - Commit message mentions the task number (e.g., "Task 1.2")
   - Commit touches files listed in the task's Files section
   - Either match is sufficient
6. Score: `completed_tasks / tasks_scheduled_through_current_wave` (uses tasks expected by now, not total plan tasks, to avoid false drift in early waves)
7. Reset `phases_since_check` to 0, increment `checks_performed`

**If drift warning (score 0.5-0.8):**

Set `alignment.drift_detected: true`, `alignment.drift_details` to missing tasks list. Increment `alignment.consecutive_drifts`. Present to user via AskUserQuestion:

```
Direction check after phase {N} - DRIFT WARNING ({score}%):
  Planned: {total} tasks
  Evidence of completion: {completed} tasks
  Missing: {list of missing tasks}

Options:
  1. Continue - I'll address missing items in upcoming phases
  2. Pause - Let me re-read the plan and adjust approach
  3. BLOCK - I need your input on the missing items
```

**If major drift (score < 0.5):**

Same as warning but with BLOCK as the recommended option:

```
Direction check after phase {N} - MAJOR DRIFT ({score}%):
  Planned: {total} tasks
  Evidence of completion: {completed} tasks
  Missing: {list of missing tasks}

Options:
  1. BLOCK - I need your input on the missing items (recommended)
  2. Pause - Let me re-read the plan and adjust approach
  3. Continue anyway - I'll try to address everything remaining
```

**If aligned (score >= 0.8):**

Log: "Direction check passed ({score}%). Continuing." Reset `alignment.drift_detected: false`, `alignment.consecutive_drifts: 0`.

See [state-schema.md](state-schema.md) for full alignment tracking fields.

**Critical for Phase 6:** If `no_ship: true`, skip this phase entirely. Set `artifact_shipped: skipped` and `pr_number: null`. Also set `external_review_passed: skipped` (no PR to review). Otherwise, MUST capture `pr_number` from output. If null, STOP and retry `/pr create`.
