# State File Formats

Reference for STATE.md structure used by /fno:do waves.

## STATE.md Schema

```yaml
# Header
updated: ISO-8601 timestamp
plan: path/to/plan/folder
execution_mode: sequential | parallel | mixed

# Wave Progress
waves:
  - number: 1
    status: COMPLETE | IN_PROGRESS | PENDING | FAILED
    started: ISO-8601 (optional)
    completed: ISO-8601 (optional)
    tasks:
      - id: "1.1"
        status: COMPLETE | IN_PROGRESS | PENDING | FAILED
        executor_id: string (optional)
        result: SUCCESS | FAILED | TIMEOUT (optional)
        error: string (optional)

# Current Position
current_wave: number
current_task: string (optional, for sequential)

# Verification
verification:
  status: PASS | FAIL | PARTIAL | NOT_RUN
  last_run: ISO-8601 (optional)
  issues: list (optional)
```

## Example STATE.md

```markdown
---
updated: 2026-01-24T14:30:00Z
plan: internal/fno/plans/2026-01-24-phase7-consolidation
execution_mode: mixed
---

# Execution State

## Wave Progress

### Wave 1: Foundation (COMPLETE)
Started: 2026-01-24T14:00:00Z
Completed: 2026-01-24T14:15:00Z

- [x] 1.1: Update /blueprint skill - SUCCESS
- [x] 1.2: Internalize wave-analyzer - SUCCESS

### Wave 2: Core Rewrite (IN_PROGRESS)
Started: 2026-01-24T14:16:00Z

- [x] 2.1: Rewrite /do core - SUCCESS
- [ ] 2.2: Add error handling - IN_PROGRESS

### Wave 3: References (PENDING)

- [ ] 3.1: wave-patterns.md
- [ ] 3.2: error-recovery.md
- [ ] 3.3: state-formats.md

### Wave 4: Documentation (PENDING)

- [ ] 4.1: Update CLAUDE.md
- [ ] 4.2: Deprecate old skills
- [ ] 4.3: Update target

## Current Position

Wave: 2
Task: 2.2
Mode: Sequential

## Verification

Status: NOT_RUN
```

## Update Patterns

### After Task Completion

```markdown
# Before
- [ ] 2.1: Rewrite /do core - PENDING

# After
- [x] 2.1: Rewrite /do core - SUCCESS
```

### After Wave Completion

```markdown
# Before
### Wave 2: Core Rewrite (IN_PROGRESS)
Started: 2026-01-24T14:16:00Z

# After
### Wave 2: Core Rewrite (COMPLETE)
Started: 2026-01-24T14:16:00Z
Completed: 2026-01-24T14:45:00Z
```

### After Failure

```markdown
# Task failure
- [!] 2.2: Add error handling - FAILED
  Error: TypeError at line 45

# Wave with partial failure
### Wave 2: Core Rewrite (PARTIAL)
```

## Parsing STATE.md

Python helper for reading state:

```python
def parse_state(state_path: str) -> dict:
    content = Path(state_path).read_text()

    # Parse YAML frontmatter
    frontmatter = yaml.safe_load(
        re.search(r'^---\n(.*?)\n---', content, re.DOTALL).group(1)
    )

    # Parse task statuses
    tasks = {}
    for match in re.finditer(r'- \[(.)\] ([\d.]+[a-z]?): .* - (\w+)', content):
        marker, task_id, status = match.groups()
        tasks[task_id] = {
            'complete': marker == 'x',
            'failed': marker == '!',
            'status': status
        }

    return {
        'updated': frontmatter.get('updated'),
        'plan': frontmatter.get('plan'),
        'tasks': tasks
    }
```

## Task Executor Return Contract

The archer subagent MUST return a structured result that the orchestrator can parse:

**Success format:**
```
RESULT: SUCCESS
TASK: 2.1
COMMIT: abc1234
SUMMARY: Implemented auth endpoints with JWT validation
```

**Failure format:**
```
RESULT: FAILURE
TASK: 2.2
ERROR: Test failure in auth.spec.ts
DETAILS: Expected 200, got 401 - missing token validation
```

This contract enables the orchestrator to:
- Parse success/failure status
- Extract task ID for STATE.md updates
- Display meaningful error details to user

## Scratchpad Wave Results

After updating STATE.md for each completed wave, also write to scratchpad:

```bash
SCRATCHPAD=$(sed -n 's/^scratchpad_path:[[:space:]]*//p' .fno/target-state.md 2>/dev/null)
if [[ -n "$SCRATCHPAD" && -d "$SCRATCHPAD/execution" ]]; then
  cat > "$SCRATCHPAD/execution/wave-${WAVE_NUM}-results.md" << EOF
## Wave ${WAVE_NUM}: ${WAVE_REASON}
Mode: ${WAVE_MODE}
Completed: $(date -u +%Y-%m-%dT%H:%M:%SZ)

## Task Results
${TASK_RESULTS}

## Files Changed
$(git diff --name-only HEAD~${COMMITS_IN_WAVE})

## Issues Encountered
[Any deviations, concerns, or blocked tasks]
EOF
fi
```

This is fire-and-forget. If the scratchpad does not exist (manual /do waves
invocation outside archer), skip silently.

## Best Practices

1. **Update atomically** - Complete write before moving to next step
2. **Timestamp everything** - Helps debug timing issues
3. **Include error details** - Stack traces, file locations
4. **Keep format consistent** - Use templates, not ad-hoc
5. **Commit state files** - Part of project history (or gitignore if local-only)
