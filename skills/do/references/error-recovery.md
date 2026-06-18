# Error Recovery

Reference for handling failures during wave execution.

## Failure Types

### 1. Single Task Failure (Sequential Wave)

**Scenario:** Task 2.2 fails during sequential execution

**STATE.md shows:**
```markdown
- [x] 2.1: Create API endpoint - COMPLETE
- [!] 2.2: Add error handling - FAILED
- [ ] 2.3: Write tests - BLOCKED
```

**Recovery:**
1. Review failure output
2. Fix the issue manually or with `/fix investigate`
3. Resume: `/do --resume`

### 2. Partial Wave Failure (Parallel Wave)

**Scenario:** 2 of 3 parallel tasks fail

**STATE.md shows:**
```markdown
Wave 2 (parallel):
- [x] 2.1: Auth module - COMPLETE
- [!] 2.2: Billing module - FAILED
- [!] 2.3: Notifications - FAILED
```

**Recovery Options:**

**Option A: Retry Failed Tasks**
```
/do --retry 2.2
/do --retry 2.3
```
Retry each failed task individually.

**Option B: Retry Entire Wave**
```
/do --retry-wave 2
```
Re-runs all tasks in wave 2 (successful ones will be quick).

**Option C: Manual Fix + Resume**
1. Fix issues manually
2. Update STATE.md to mark tasks complete
3. `/do --resume`

### 3. Executor Timeout

**Scenario:** Task executor doesn't respond within timeout

**Recovery:**
1. Check if executor is still running (Task tool status)
2. If stuck, kill and retry: `/do --kill-and-retry 2.1`
3. If persistent, investigate resource constraints

### 4. Verification Failure

**Scenario:** All tasks complete but verifier reports FAIL

**STATE.md shows:**
```markdown
## All Waves Complete

## Verification
Status: FAIL
Issues:
- AC2-ERR: Error handling not implemented
- Tests: 2 failures in auth.spec.ts
```

**Recovery:**
1. Do NOT claim "done"
2. Review verification output
3. Fix issues
4. Re-run verification: `/do --verify-only`
5. If PASS, then report completion

### 5. Context Exhaustion

**Scenario:** Main context approaching limit during orchestration

**Symptoms:**
- Slow responses
- Truncated tool results
- Orchestrator losing track of wave state

**Recovery:**
1. Ensure STATE.md is current
2. Trigger context compaction (new conversation)
3. `/do --resume` in fresh context

## Recovery Commands Reference

| Command | Action |
|---------|--------|
| `/do --resume` | Continue from last checkpoint in STATE.md |
| `/do --retry <task>` | Re-run specific task |
| `/do --retry-wave <n>` | Re-run entire wave |
| `/do --verify-only` | Run verification without execution |
| `/do --status` | Show current STATE.md summary |

## Prevention

### Avoid Failures

1. **Write detailed PLAN.md** - Executor has clear instructions
2. **Set appropriate timeouts** - Complex tasks need more time
3. **Test locally first** - Catch issues before executor runs
4. **Keep context lean** - Offload to subagents early

### Monitor Progress

During execution, check:
```bash
cat .fno/STATE.md
```

Watch for:
- Stalled waves (no progress)
- Repeated failures on same task
- Context size warnings

## Task Attempt Tracking

Before dispatching a task, check its attempt count in STATE.md:

```yaml
task_attempts:
  "2.3": 2  # This task has failed twice
```

**Rules:**
- If attempt count >= 3: Mark task BLOCKED, skip to next task in wave
- If attempt count < 3: Dispatch normally, increment counter on FAILURE
- On SUCCESS: Remove task from `task_attempts` (reset counter)

**Dependency Cascade:**
When marking a task BLOCKED, scan remaining waves for dependent tasks:
- If a later wave's tasks depend on the blocked task's phase, mark them BLOCKED too
- Reason: `"Upstream dependency [task-id] is BLOCKED"`

## Partial Wave Failure (Detailed)

When some tasks in a parallel wave fail:

1. **Collect Results** - Note which succeeded, which failed
2. **Update STATE.md** - Mark succeeded tasks complete, failed with `[!]`
3. **Report Clearly** - Show exact failures with details
4. **Offer Options**:
   - Retry failed tasks: `/do waves --retry 2.2`
   - Skip and continue: `/do waves --continue`
   - Abort: User decides next steps

### Error Report Format

```markdown
## Wave 2 Partial Failure

**Execution Mode:** parallel (3 tasks)

**Results:**
| Task | Status | Details |
|------|--------|---------|
| 2.1 | SUCCESS | Completed |
| 2.2 | FAILED | TypeError at auth.ts:45 |
| 2.3 | SUCCESS | Completed |

**Failure Details:**
Task 2.2 failed with:
TypeError: Cannot read property 'user' of undefined
  at authenticate (auth.ts:45)

**Options:**
1. `/do waves --retry 2.2` - Re-run failed task only
2. `/do waves --continue` - Skip to wave 3 (not recommended)
3. Manual fix then `/do waves --resume`
```

### Task Retry

```
/do waves --retry <task-id>

Behavior:
1. Spawn single archer for specified task
2. On success, update STATE.md
3. If wave now complete, continue to next wave
```

## When to Escalate

Some failures need human intervention:

- **Persistent test failures** - Might need design change
- **Resource limits hit** - Might need infrastructure change
- **Circular dependencies** - Might need plan restructure

In these cases, report clearly and await user decision.
