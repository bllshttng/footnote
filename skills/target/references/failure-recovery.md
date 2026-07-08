# Failure Recovery

**Load when:** any failure during execute, review, or validate. Covers validation-failure recovery, circuit breaker, and the standard error-handling responses.

## Validation Failure Recovery (Phase 5)

When validation fails:

1. Track consecutive failures in-session (no state file writes needed).
2. If fewer than 3: continue fixing forward.
3. If 3 or more AND a checkpoint exists:
   - Present to user via AskUserQuestion: "Build has failed 3 times since checkpoint. Options:
     1. Rollback to pre-execute checkpoint and retry with different approach
     2. Continue fixing forward
     3. Pause for manual intervention (writes the cancel sentinel)"
   - If rollback chosen:
     ```bash
     source "${CLAUDE_PLUGIN_ROOT}/scripts/lib/checkpoint.sh"
     rollback_checkpoint "${CHECKPOINT_NAME}"
     ```
4. If 3 or more AND no checkpoint: touch `.fno/.target-cancelled` (cancel sentinel); the loop-check verb will terminate with `Interrupted` on next stop.

**On validation success:** Reset in-session counter. Run checkpoint cleanup:

```bash
source "${CLAUDE_PLUGIN_ROOT}/scripts/lib/checkpoint.sh"
cleanup_checkpoints 3
```

## Circuit Breaker Check (All Failure Points)

After any failure during execute/review/validate phases, check for burn loops:

1. Generate error signature: lowercase the error, strip file paths (keep filename only), strip line numbers, take first 100 chars. If result is empty, use `unknown-error-exit-{code}`.
2. Track in-session: compare to the last seen error signature. If this is the first failure, treat as "different".
3. If **same** signature:
   - Increment in-session consecutive_same_error counter
   - Track current approach in approaches_tried list
4. If **different** signature:
   - Reset counter to 1
   - Update the tracked error signature
   - Reset approaches_tried to current approach only

**If `consecutive_same_error >= 3` (circuit trips):**

Set `circuit_breaker.tripped: true`, increment `circuit_breaker.trip_count`, and present to user via AskUserQuestion:

```
Circuit breaker tripped - same error 3 times:
  {error_signature}

Approaches tried:
  1. {approaches_tried[0]}
  2. {approaches_tried[1]}
  3. {approaches_tried[2]}

Options:
  1. Try a completely different approach (root cause re-analysis)
  2. Skip this task and continue with the next one
  3. BLOCK - pause for manual intervention
  4. Reset circuit breaker and keep trying (override)
```

- **Option 1:** Reset `consecutive_same_error`, set `tripped: false`, retry with prompt: "Previous approach failed 3 times with: {signature}. Tried: {approaches}. FORBIDDEN: repeating any listed approach. Analyze root cause from scratch."
- **Option 2:** Mark current task as skipped, move to next task/wave
- **Option 3:** Touch `.fno/.target-cancelled` (cancel sentinel — the loop-check verb will terminate with `Interrupted` on next stop).
- **Option 4:** Reset `consecutive_same_error`. If `trip_count >= 3`, touch `.fno/.target-cancelled` to hand off to the user.

**On any success:** Reset `circuit_breaker.consecutive_same_error` to 0, clear `tripped`, clear `approaches_tried` to `[]`.

**On checkpoint rollback:** Also reset `circuit_breaker.consecutive_same_error` to 0 and clear `approaches_tried` (rolled-back code invalidates prior approach tracking).

**Precedence with checkpoint rollback:** When both conditions trigger simultaneously (`consecutive_failures >= 3` AND `consecutive_same_error >= 3`), check circuit breaker first. If it handles the failure (rotate/skip/restart), suppress the checkpoint rollback prompt for that failure event. Circuit breaker is more specific (same error) so it takes priority.

## Session termination diagnostics

The loop-check verb emits a `termination` event to `.fno/events.jsonl` with a `TerminationReason` when it stops the session. Use `fno status` to read the latest termination reason:

| `TerminationReason` | Cause | Recovery |
|---|---|---|
| `NoProgress` | 4-component fingerprint (HEAD sha, PR state, CI conclusion, review ts) unchanged for N fires | Address the root cause; re-run `/target` |
| `Budget` | Wall-clock or cost cap reached | Raise the cap in config.toml; re-run `/target` |
| `Interrupted` | `.fno/.target-cancelled` sentinel touched | User-initiated; remove sentinel and re-run |
| `DonePRGreen` | PR green + reviewed (success) | No action needed |
| `DoneAdvisory` | Advisory mode completion (no_ship) | No action needed |
| `Aborted` | `<aborted reason="...">` emitted by session | Read the abort reason; address the kill criteria |
| `NoWork` | No state file or no active work | Start a fresh session with `/target` |

### Orphaned-session bound (credit-burn backstop)

A separate detector (`scripts/lib/orphan-target-detector.sh`) blocks exit when the transcript shows a genuine `/fno:target` or `/fno:megawalk` invocation but the corresponding state file (`target-state.md` / `megawalk-state.md`) was never created, i.e. init was skipped and all completion gates were bypassed. It re-fires on every stop while that condition holds, so an unattended session with no human to type `/fno:target cancel` would loop forever and burn credits. To bound that, the detector counts consecutive orphan blocks for the conversation and, past the limit (`TARGET_ORPHAN_BLOCK_LIMIT`, default 5 attended / 3 unattended), allows a clean exit while loudly recording the gate-bypass as an `orphaned_target_abandoned` event.

The counter is anchored to **(repo, conversation)**: keyed by the session UUID and stored under the root of the repo's shared git common dir (`git rev-parse --git-common-dir`), so every worktree of the repo reads one monotonic count. This is what makes the bound release **durably**: when one conversation fires Stop from two divergent worktree cwds (the same-transcript two-worktree ambiguity), a per-cwd counter would fork and never reach the limit in both, so the bound never released (ab-32076fa0). Resolution falls back to the legacy per-cwd `.orphan-block-count` when no session id is present, to `TARGET_ORPHAN_BLOCK_DIR` when that override is set, and to `~/.fno/orphan-blocks` outside a git repo. If the counter cannot be persisted at all (read-only FS, disk full, sparse hook env), the bound is structurally defeated, so it **fails open** (allows a clean exit, logged as `orphan_counter_unwritable`) rather than blocking forever.

A deliberate `/fno:target cancel` writes a session-keyed `.target-cancelled-final` tombstone that this detector honors before any block, so cancelling does not trip it.

## Standard Error Responses

### /do waves Returns FAILED

```markdown
## Iteration N - Blocked

/do waves reported failure:
- Task: 2.2
- Error: {error_details}

**Action:** Fix the issue, then re-run `/target resume`
```

### Review Finds Issues

```markdown
## Iteration N - Review Failed

Issues found:
- {issue_1}
- {issue_2}

**Action:** Fixing issues and re-running review...
```
