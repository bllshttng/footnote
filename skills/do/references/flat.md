# Do — Flat Mode (lightweight single-session)

Lightweight executor for focused plans. Read the plan, make the changes, verify, done. You ARE the executor - no subagents, no STATE.md, no wave parsing.

## 0. Structural Context (if available)

Read `.fno/codemap.md` if it exists. Do NOT generate it - flat mode is lightweight and shouldn't add 3 seconds to every invocation. If the user ran `fno codemap`, `/blueprint`, or `/target` previously, the file will be there. Use it to understand which files are high-importance (top of the codemap output = highest PageRank) before making changes.

## 1. Read Plan

Read the plan file. Expect the focused plan format:
- `## Context` — understand the problem
- `## Changes` — numbered changes with files
- `## Files to Modify` — quick reference table
- `## Patterns to Reuse` — (optional) existing code to follow
- `## Verification` — how to confirm success

## 1b. Resolve the executor — don't let frontend work skip the craft pass

Before editing, check whether the plan touches **frontend surfaces**: any
file matching the locked list `*.tsx`, `*.jsx`, `components/**`,
`routes/**`, or `src/styles/**` (the same list waves mode routes on;
backend `app/` modules do not match).

If it does, and a frontend-craft executor is installed (the `impeccable`
skill), run the craft pass on those files with `/impeccable` rather than
hand-editing them as plain code.

If no frontend-craft executor is installed, plain execution is fine.

## 2. Execute Changes

**Before each numbered change**, evaluate the plan's `## Kill Criteria`
fenced YAML block (if present). If a predicate fires, stop, emit
`<aborted reason="{name}">MISSION ABORTED: {reason}</aborted>`, and do
NOT make further changes:

```bash
PLAN_PATH="$1"   # the .md file path passed to /do
# kill-criteria.sh was folded into the fno-agents binary (US1, ab-58645f63).
# `fno phase kill-check` prints `KILL_CRITERIA_FIRED <name>|<reason>` and exits
# 1 when a predicate fires, exits 0 (empty) when none fire, and exits 2 when the
# fno-agents binary is unavailable. Branch on the exit code: only rc 1 WITH the
# marker aborts; rc 2 (or any other non-zero) is an infra failure that warns and
# skips, never aborting /do with an empty kill reason (codex PR #515 P2).
if [[ -n "$PLAN_PATH" ]] && command -v fno >/dev/null 2>&1; then
    KC_OUT=$(fno phase kill-check "$PLAN_PATH" 2>/dev/null); KC_RC=$?
    if [[ $KC_RC -eq 1 && "$KC_OUT" == KILL_CRITERIA_FIRED* ]]; then
        KC_NAME="${KC_OUT#KILL_CRITERIA_FIRED }"; KC_NAME="${KC_NAME%%|*}"
        KC_REASON="${KC_OUT##*|}"
        echo "do: kill_criteria fired - $KC_NAME: $KC_REASON" >&2
        # Emit <aborted> in user-facing output and stop.
    elif [[ $KC_RC -ne 0 ]]; then
        echo "do: kill-check unavailable (rc=$KC_RC); skipping kill-criteria gate" >&2
    fi
fi
```

Backward compat: plans without a `## Kill Criteria` block (most focused
plans will not have one) return exit 0 (no abort). Malformed predicates
log WARN to stderr and are skipped.

**Session-project invariant:** a flat plan is single-project by construction.
If a numbered change would edit a file **outside this session's project repo
root**, STOP — do NOT `cd` into the other repo and edit it. Surface that work as
a backlog node and spawn a worker into its project
(`fno agents spawn --harness claude --cwd <root> "target-<node>" "/target <node>"`),
or, if no node exists yet, report it so the user can `/blueprint` it. See
[session-project-invariant.md](session-project-invariant.md).

For each numbered change under `## Changes`:

1. **Read** the target file(s) listed in the change
2. **Read** any "Patterns to Reuse" sources referenced
3. **Make** the change as described
4. **Test** if a quick check is available (typecheck, related test file)
5. **Commit** atomically - one commit per numbered change, scoped to that change's files only. Never `git add .` or defer commits to the end.

Work sequentially through the numbered changes. If a change depends on a previous one, the numbering handles ordering.

**FORBIDDEN:** Accumulating all changes into a single commit at the end. Each numbered change gets its own commit as soon as it passes its test.

## 3. Verify

Run every step listed under `## Verification`:
- Commands → execute and check output
- Behavioral checks → verify manually or describe result
- If any verification fails → stop and report what failed

## 3b. Status-breakpoint emit (x-dbaf, best-effort)

After each change lands (or blocks), emit the task-boundary event so observers can track the run. `task_started` already fired from `resolve-executor.sh` at dispatch; `run_summary` fires from `finalize` at the loop terminal. You emit the middle boundary — one non-fatal line per change, never gating anything:

```bash
# SUCCESS / DONE_WITH_CONCERNS after a change commits:
python3 "${SKILL_DIR:-skills/do}/orchestrator.py" --emit-boundary task_done \
  --task "<n>" --outcome SUCCESS --data '{"commit":"<sha>","concerns":0}'
# BLOCKED when a change cannot proceed (also the <help> path):
python3 "${SKILL_DIR:-skills/do}/orchestrator.py" --emit-boundary blocked \
  --task "<n>" --data '{"reason":"<why>"}'
```

`--run`/`--node` fall back to the manifest, so they are optional here. Emission is best-effort: a failure prints one stderr note and never stops the run.

## 3c. Revision-round crumb (builder trail, best-effort)

Only when a change needed a **revision round** - its verify/test failed and you reworked it before it landed - drop ONE `builder_step` into `.fno/events.jsonl` so a resume or self-handoff successor sees what was tried and how it was fixed. A clean first-pass change emits nothing here: `task_started` + `task_done` already describe it fully. One crumb per reworked change, at the boundary - never per edit.

```bash
fno event emit --type builder_step \
  -d '{"tried":"<the first approach>","found":"<why it failed verify>","fix":"<what made it pass>","outcome":"worked"}' \
  || echo "warning: builder_step crumb not recorded (continuing)" >&2
```

Truncate `tried`/`found`/`fix` to ~500 chars each. `tried` and `outcome` are required (`worked|failed|abandoned`); `found`/`fix` are optional. Degrade, never block: a failed emit prints exactly the one warning and execution continues.

## 4. Report Done

Summarize what was done:
```
## Done

- Change 1: [what was done]
- Change 2: [what was done]
- Verification: [pass/fail summary]
```

## What flat mode does NOT do

- Spawn subagents (you ARE the executor)
- Track state in STATE.md
- Parse execution strategy YAML or waves
- Retry failed tasks automatically
- Integrate with Linear
- Enforce BDD acceptance criteria
- Handle parallel execution

If you need any of the above, use `/do waves` instead.

## TDD Behavior

Lightweight, not forced:
- If a test file exists for the code being changed, run it after changes
- If the project has `test_command` in config.toml, use it
- No mandatory red-green-refactor protocol
- Focus on "does it work?" not "did we follow the ceremony?"

## Error Handling

- **Change fails:** Stop, report which change failed and why. Don't continue blindly.
- **Verification fails:** Report the specific step that failed. Suggest a fix if obvious.
- **Plan unclear:** Ask the user rather than guessing. The plan should be self-contained, but if it's not, surface the ambiguity.
