# Verification Protocol

Fresh-perspective verification that checks plan-vs-implementation alignment
after all waves complete. Complements the existing fno:verifier
(which checks acceptance criteria and test results).

## Purpose

The existing verifier checks "did tests pass."
This verification checks "did we build what was planned."
These are different questions. Both must pass.

## Sequencing

1. Run existing fno:verifier first (acceptance criteria check)
2. If it passes, run this fresh verification
3. If it fails, fix first before running fresh verification

## Dispatch

- **Agent:** archer
- **Tools:** `["Read", "Grep", "Glob", "Bash"]` (Bash for git diff only)

### Prompt Template

```
You are verifying that the implementation matches the plan.

Plan: {path to plan .md}
Changes: run `git diff {base_commit}..HEAD --stat` to see what changed

For each task in the plan:
1. Read the task's acceptance criteria
2. Read the files that were supposed to be modified
3. Check: does the implementation satisfy the intent?
4. Check: are there files modified that aren't in the plan? (scope creep)
5. Check: are there plan tasks with no corresponding changes? (missed work)

Report format:
## Verification Report
### Tasks Verified
- Task X.Y: PASS | FAIL | PARTIAL - [reason]
### Scope Check
- Unplanned files modified: [list or "none"]
- Planned tasks with no changes: [list or "none"]
### Gaps Found
- [gap description and severity]
### Verdict: PASS | FAIL
```

## After Verification

- **PASS:** Proceed to adversarial challenge (if `--adversarial` flag set) or report completion
- **FAIL:** Report gaps to user (interactive mode) or attempt fix (under target)

## Coordinator State

Update `coordinator_phase` in target-state.md:
- Set to `verification` before dispatching the verification worker
- Set to `complete` (or `adversarial` if flag set) when verification finishes

On resume: if `coordinator_phase: verification`, re-run verification
(it reads current git state, so it is always up to date).
