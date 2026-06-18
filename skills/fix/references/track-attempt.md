# Attempt Tracker

Track all debugging attempts to prevent repeating failed approaches and accelerate bug resolution.

## Read Before Investigating

Before forming hypotheses, check prior attempts:

```bash
grep -i "error-keyword" .debug/attempts.jsonl
```

If prior attempts exist:
- Skip hypotheses already marked "failed"
- Start with hypotheses that were "promising but untested"
- Note in STATE.md: "Skipping X (failed on Y date)"

## Write After Each Attempt

Append to `.debug/attempts.jsonl` after every debugging attempt:

```jsonl
{
  "ts": "ISO timestamp",
  "bug": "bug-identifier",
  "stage": "single|expansion|tournament",
  "hypothesis": "What was tried",
  "outcome": "failed|solved|investigating|skipped",
  "finding": "What was learned",
  "duration_min": N,
  "agent": "agent-id (if tournament)"
}
```

### Field Definitions

| Field | Required | Description |
|-------|----------|-------------|
| `ts` | Yes | ISO 8601 timestamp (e.g., `2026-01-23T14:30:00Z`) |
| `bug` | Yes | Unique identifier for the bug (e.g., `auth-timeout-500`) |
| `stage` | Yes | Debugging stage: `single`, `expansion`, or `tournament` |
| `hypothesis` | Yes | What approach was tried |
| `outcome` | Yes | Result: `failed`, `solved`, `investigating`, or `skipped` |
| `finding` | Yes | What was learned from this attempt |
| `duration_min` | No | How long the attempt took in minutes |
| `agent` | No | Agent ID if running in tournament mode |

### Outcome Values

- **failed**: Hypothesis did not resolve the bug
- **solved**: Bug was resolved by this approach
- **investigating**: Still in progress, partial findings
- **skipped**: Hypothesis skipped due to prior failure or irrelevance

## File Location

```
.debug/                       # At project root
└── attempts.jsonl            # Line-delimited JSON of all attempts
```

## Process

### 1. Ensure Directory Exists

```bash
mkdir -p .debug
```

### 2. Check for Prior Attempts

Before investigating a new bug:

```bash
# Search for similar errors
grep -i "error-keyword" .debug/attempts.jsonl 2>/dev/null

# Search for specific bug
grep "bug-identifier" .debug/attempts.jsonl 2>/dev/null
```

### 3. Record New Attempt

After each debugging attempt:

```bash
# Get current timestamp
ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

# Append attempt to log (single line, no pretty print)
echo '{"ts":"'"$ts"'","bug":"auth-timeout","stage":"single","hypothesis":"Check connection pool exhaustion","outcome":"failed","finding":"Pool size was adequate, connections not the issue"}' >> .debug/attempts.jsonl
```

### 4. Update STATE.md

When skipping a hypothesis due to prior failure:

```markdown
## Debugging Notes
- Skipping "connection pool exhaustion" (failed 2026-01-20, finding: pool size adequate)
- Trying: "socket timeout configuration" (untested)
```

## Deduplication

If a new bug has the same error message as a prior bug:

1. **Load prior attempts:**
```bash
grep "same-error-message" .debug/attempts.jsonl
```

2. **Show user the history:**
```
This error was seen before. Prior investigation found:
- Hypothesis A: Failed - [finding]
- Hypothesis B: Failed - [finding]
- Hypothesis C: Solved - [finding]
```

3. **Suggest acceleration:**
```
Skip to tournament with known-bad hypotheses excluded?
- Will exclude: Hypothesis A, Hypothesis B
- Will prioritize: Variations of Hypothesis C
```

## Integration

This skill is used by:

| Skill/Agent | How It Uses Attempt Tracker |
|-------------|---------------------------|
| `debug` | Reads prior attempts before starting |
| `tournament-debugger` | Records each agent's attempt and outcome |
| `/fno:do waves` | References attempt log in STATE.md |

## Example Session

### Recording a Failed Attempt

```bash
mkdir -p .debug
echo '{"ts":"2026-01-23T10:30:00Z","bug":"api-500-users","stage":"single","hypothesis":"Check null user ID handling","outcome":"failed","finding":"User ID validation is correct, error occurs after validation"}' >> .debug/attempts.jsonl
```

### Recording a Successful Resolution

```bash
echo '{"ts":"2026-01-23T11:15:00Z","bug":"api-500-users","stage":"single","hypothesis":"Check database connection retry logic","outcome":"solved","finding":"Connection retry was not exponential, causing cascade failures under load","duration_min":45}' >> .debug/attempts.jsonl
```

### Recording Tournament Agent Attempts

```bash
echo '{"ts":"2026-01-23T12:00:00Z","bug":"memory-leak-dashboard","stage":"tournament","hypothesis":"Check useEffect cleanup","outcome":"failed","finding":"All effects have proper cleanup","agent":"agent-1"}' >> .debug/attempts.jsonl

echo '{"ts":"2026-01-23T12:05:00Z","bug":"memory-leak-dashboard","stage":"tournament","hypothesis":"Check event listener accumulation","outcome":"solved","finding":"Window resize listener added on every render, not cleaned up","agent":"agent-3"}' >> .debug/attempts.jsonl
```

### Querying Prior Attempts

```bash
# All attempts for a specific bug
grep "memory-leak-dashboard" .debug/attempts.jsonl | jq .

# All failed attempts
grep '"outcome":"failed"' .debug/attempts.jsonl

# All attempts by a specific agent
grep '"agent":"agent-1"' .debug/attempts.jsonl

# Count attempts per bug
cat .debug/attempts.jsonl | jq -r '.bug' | sort | uniq -c
```

## Best Practices

1. **Write immediately** - Record attempt right after completing it, not later
2. **Be specific in findings** - "Didn't work" is not helpful; explain why
3. **Use consistent bug IDs** - Same bug should have same identifier across attempts
4. **Include duration** - Helps calibrate time estimates for similar bugs
5. **Search before investigating** - Always check prior attempts first
6. **Update STATE.md** - Note skipped hypotheses and their reasons
