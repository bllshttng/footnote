# Wave Execution Patterns

Reference for determining when to use sequential vs parallel execution in waves.

## Decision Tree

```
Does wave have file conflicts?
├── YES → Sequential
└── NO → Do tasks share hidden output roots (.fno/, .codex/agents/, docs/, runtime metadata)?
         ├── YES → Sequential
         └── NO → Does provider capability contract allow subagents + parallel dispatch?
                  ├── NO → Sequential fallback
                  └── YES → Parallel is safe
```

## Pattern: Sequential Wave

**When to use:**
- Tasks modify the same file
- Task B reads output of Task A
- Database migrations must run in order
- Tests depend on implementation

**Example:**
```yaml
- wave: 1
  mode: sequential
  tasks: [1.1, 1.2]
  reason: "1.2 adds foreign key to table created in 1.1"
```

**Behavior:**
- Spawn executor for task 1.1
- Wait for completion
- Spawn executor for task 1.2
- Wait for completion
- Update STATE.md

## Pattern: Parallel Wave

**When to use:**
- Tasks touch completely different files
- No data dependencies between tasks
- Different feature areas
- Independent test suites
- No shared generated outputs, docs roots, or runtime metadata directories
- Provider advertises `supports_subagents: true` and `supports_parallel_dispatch: true`

**Example:**
```yaml
- wave: 2
  mode: parallel
  tasks: [2.1, 2.2, 2.3]
  reason: "Auth, billing, and notifications are independent features"
```

**Behavior:**
- Spawn 3 executors simultaneously
- Wait for ALL to complete
- Collect results
- Handle any failures
- Update STATE.md

## Pattern: Mixed Execution

**When to use:**
- Foundation work must be sequential
- Feature work can be parallel
- Some waves have dependencies, others don't

**Example:**
```yaml
execution_mode: mixed

waves:
  - wave: 1
    mode: sequential
    tasks: [1.1]
    reason: "Database setup - foundation"

  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2, 2.3]
    reason: "Independent feature modules"

  - wave: 3
    mode: sequential
    tasks: [3.1]
    reason: "Integration tests need all features"
```

## Common Mistakes

### Over-Parallelization

**Problem:** Running tasks in parallel when they share resources
```yaml
# WRONG - both modify routes/index.tsx
- wave: 2
  mode: parallel
  tasks: [2.1, 2.2]  # Both add routes to same file
```

**Fix:** Make sequential or merge into single task

### Under-Parallelization

**Problem:** Running independent tasks sequentially
```yaml
# INEFFICIENT - no dependencies between these
- wave: 2
  mode: sequential
  tasks: [2.1, 2.2, 2.3]  # Each touches different files
```

**Fix:** Change to parallel mode

### File Conflict Detection

Check for conflicts before assigning parallel mode:

```bash
# List files touched by each task
grep -A 10 "Files:" 02-phase.md

# If any file appears in multiple tasks
# → Those tasks must be sequential
```

Also check hidden shared outputs:

- `.fno/`
- `.codex/agents/`
- `docs/`
- `internal/`
- `runtime/`

## Quick Reference

| Situation | Mode | Why |
|-----------|------|-----|
| Same file, multiple tasks | sequential | Avoid merge conflicts |
| Shared output root | sequential | Generated artifacts can race even when primary files differ |
| Migration ordering | sequential | Database integrity |
| Implementation + tests | sequential | Can't test unwritten code |
| Provider disables subagents | sequential | Capability-driven fallback |
| Different features + capable provider | parallel | No resource sharing |
| Same prefix (02a, 02b) | parallel | Convention indicates independence |
| Fan-in dependency | sequential in final wave | Must wait for all prior |

## Pattern: Dynamic Parallelization

**When it activates:**
Plan has a `## File Ownership Map` section and execution
strategy has at least one sequential wave.

**What it does:**
Parses file ownership map into task-to-files mappings. For each sequential
wave, checks if task file sets are disjoint. Upgrades disjoint waves to parallel.

**Rules:**
- Only upgrades, never downgrades
- Tasks missing from map are treated as touching all files (conservative)
- See `dynamic-parallelization.md` for the full algorithm

**Extended Decision Tree:**

```
Is wave declared sequential?
+-- NO (parallel) -> Leave as-is
+-- YES -> Does the plan have a File Ownership Map?
         +-- NO -> Keep sequential (declared strategy)
         +-- YES -> Are all tasks present in the map?
                  +-- NO -> Keep sequential (unknown scope)
                  +-- YES -> Are all task file sets disjoint?
                           +-- NO -> Keep sequential
                           +-- YES -> Upgrade to parallel
```
