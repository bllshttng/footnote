# Resume Execution Protocol

If execution was interrupted:

## Check for Existing State

```bash
cat .fno/STATE.md 2>/dev/null || echo "No state found"
```

## Parse Completed Tasks

From STATE.md, extract:
- Completed waves (lines with `[x]`)
- Completed tasks within each wave
- Last wave in progress

## Continue from Interruption Point

1. Skip completed waves entirely
2. For partially complete wave:
   - If sequential: continue from next task
   - If parallel: re-run only failed tasks
3. Continue with remaining waves

## Resume Command

```
/do waves --resume

Behavior:
1. Read .fno/STATE.md
2. Identify completion status
3. Continue from next incomplete wave/task
```
